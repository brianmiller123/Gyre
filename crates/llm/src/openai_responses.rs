//! `OpenAI` **Responses API** 适配器（`POST {base}/responses`，wire = `Api::OpenAiResponses`）。
//!
//! 差距报告 H14：此前 `Api::OpenAiResponses` **无任何适配器**——`crates/llm/src/openai.rs`
//! 的 `SUPPORTED` 只有 Chat Completions / Ollama，而 Codex OAuth（`oauth/codex.rs:80`）
//! 存的凭据键正是 `openai-responses`：登录成功却无 wire 可用（死链）。
//!
//! 本适配器实现 Responses 线协议：
//! - 请求：`instructions`（system）+ `input` 条目数组 + 扁平化的 `tools` +
//!   `max_output_tokens` / `temperature` / `reasoning.effort` / `stream:true`。
//! - 响应：SSE 事件流（文本增量、函数参数增量、条目完成、`response.completed`）。
//! - 工具调用按 `item_id` 聚合（`call_id` 才是回填用的 id），参数增量拼接。
//! - 用量：`response.completed.response.usage`（`input_tokens` / `output_tokens` /
//!   `input_tokens_details.cached_tokens`）。
//!
//! **Codex 兼容增强**：`base_url` 指向 ChatGPT 后端（`chatgpt.com`）时附加
//! `originator` / `chatgpt-account-id` 头，并强制 `store:false` +
//! `include:["reasoning.encrypted_content"]`（ChatGPT 后端不接受存储式响应，
//! 且要求回传加密推理项）。账号 id 取自 oauth.toml（`openai-responses` 行），
//! 缺失时仅发 `originator`（标准 OpenAI 端点不需要任何附加头）。

use std::pin::Pin;

use agent_core::{
    Api, AssistantEvent, AssistantEventStream, CompletionRequest, ContentBlock, LlmError,
    LlmProvider, ProviderCallContext, ProviderMessage, StopReason, ToolChoice, ToolChoiceDirective,
    Usage, UserContent,
};
use async_stream::stream;
use futures::StreamExt;
use serde_json::{Value, json};

/// Responses API 适配器（复用共享 `reqwest::Client` 连接池）。
pub struct OpenAiResponsesAdapter {
    client: reqwest::Client,
}

impl OpenAiResponsesAdapter {
    /// 构造。
    #[must_use]
    pub const fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// 本适配器实现 Responses wire（`H14`：不再是「名义支持」）。
const SUPPORTED: &[Api] = &[Api::OpenAiResponses];

/// ChatGPT 后端（Codex OAuth）域名判定。
fn is_codex_backend(base: &str) -> bool {
    base.contains("chatgpt.com") || base.contains("chatgpt-staging")
}

/// 读取 Codex 账号 id（`oauth.toml` 的 `openai-responses` 行；缺失返回 `None`）。
fn codex_account_id() -> Option<String> {
    let dir = agent_core::platform::config_dir()?;
    agent_config::load_oauth(&dir)
        .get("openai-responses")
        .and_then(|c| c.account_id.clone())
        .or_else(|| {
            agent_config::load_oauth(&dir)
                .get("codex")
                .and_then(|c| c.account_id.clone())
        })
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiResponsesAdapter {
    fn id(&self) -> &'static str {
        "openai-responses"
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
        let url = format!("{base}/responses");
        let codex = is_codex_backend(base);
        let body = build_body(&request, codex);
        let model_id = request.model.id.clone();

        // H24：`auth = none`（或 key 为空）时不发内置鉴权头。
        let mut builder = self.client.post(&url);
        if let Some(key) = ctx.builtin_api_key() {
            builder = builder.bearer_auth(key);
        }
        builder = builder.json(&body);
        if codex {
            builder = builder
                .header("originator", "gyre")
                .header("OpenAI-Beta", "responses=experimental");
            if let Some(account) = codex_account_id() {
                builder = builder.header("chatgpt-account-id", account);
            }
        }

        let resp = crate::apply_custom_headers(builder, &ctx.headers)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(crate::http_error(resp).await);
        }
        Ok(parse_sse_stream(resp, model_id))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 请求体构建
// ──────────────────────────────────────────────────────────────────────────────

/// 构造 Responses 请求体（`codex` 为真时附加 ChatGPT 后端必需字段）。
fn build_body(req: &CompletionRequest, codex: bool) -> Value {
    let instructions = if req.system.is_empty() {
        None
    } else {
        Some(req.system.join("\n\n"))
    };

    let mut input: Vec<Value> = Vec::new();
    for msg in &req.messages {
        match msg {
            ProviderMessage::System(text) => {
                // Responses 只有单一 `instructions`；后出现的 system 追加为 developer 消息。
                input.push(json!({
                    "role": "developer",
                    "content": [{ "type": "input_text", "text": text }],
                }));
            }
            ProviderMessage::User { content } => {
                let blocks: Vec<Value> = content
                    .iter()
                    .map(|c| match c {
                        UserContent::Text { text } => {
                            json!({ "type": "input_text", "text": text })
                        }
                        UserContent::Image { mime, data } => json!({
                            "type": "input_image",
                            "image_url": format!("data:{mime};base64,{data}"),
                        }),
                    })
                    .collect();
                input.push(json!({ "role": "user", "content": blocks }));
            }
            ProviderMessage::Assistant { content } => {
                // 助手文本 → `output_text` 消息；工具调用 → 顶层 `function_call` 条目。
                let mut text_parts: Vec<Value> = Vec::new();
                for block in content {
                    match block {
                        ContentBlock::Text { text } => {
                            text_parts.push(json!({ "type": "output_text", "text": text }));
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            if !text_parts.is_empty() {
                                input.push(json!({
                                    "role": "assistant",
                                    "content": std::mem::take(&mut text_parts),
                                }));
                            }
                            input.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": arguments.to_string(),
                            }));
                        }
                        // 推理项不重放（Responses 要求加密回执，见 codex include 字段）。
                        ContentBlock::Thinking { .. } => {}
                    }
                }
                if !text_parts.is_empty() {
                    input.push(json!({ "role": "assistant", "content": text_parts }));
                }
            }
            ProviderMessage::Tool {
                tool_call_id,
                content,
                is_error,
                images,
            } => {
                let mut output = content.clone();
                if *is_error {
                    output = format!("[error] {output}");
                }
                for img in images {
                    output.push_str(&format!("\n[image:{}]", img.mime));
                }
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": output,
                }));
            }
        }
    }

    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.schema,
                "strict": false,
            })
        })
        .collect();

    let mut body = json!({
        "model": req.model.id,
        "input": input,
        "stream": true,
        "max_output_tokens": req.max_tokens,
    });
    if let Some(instructions) = instructions {
        body["instructions"] = json!(instructions);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = tool_choice(req.tool_choice.as_ref());
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(effort) = reasoning_effort(req) {
        body["reasoning"] = json!({ "effort": effort });
    }
    if codex {
        // ChatGPT 后端：不落库 + 要求回传加密推理项（保留推理链所需）。
        body["store"] = json!(false);
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    // extra_body 顶层合并（自定义网关参数），与 Chat Completions 适配器同语义。
    if let Some(extra) = req.model.extra_body.as_ref().and_then(Value::as_object) {
        if let Some(obj) = body.as_object_mut() {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    body
}

/// 思考预算 → Responses `reasoning.effort`（档位判定与 Chat Completions 适配器一致）。
fn reasoning_effort(req: &CompletionRequest) -> Option<&'static str> {
    let thinking = req.thinking.as_ref()?;
    Some(if thinking.budget_tokens >= 32_000 {
        "high"
    } else if thinking.budget_tokens >= 12_000 {
        "medium"
    } else {
        "low"
    })
}

/// 工具选择指令 → Responses `tool_choice`（无嵌套 `function` 包装）。
fn tool_choice(directive: Option<&ToolChoiceDirective>) -> Value {
    match directive {
        Some(ToolChoiceDirective::Hard(ToolChoice::Auto)) | None => json!("auto"),
        Some(ToolChoiceDirective::Hard(ToolChoice::None)) => json!("none"),
        Some(ToolChoiceDirective::Hard(ToolChoice::Any | ToolChoice::Required)) => {
            json!("required")
        }
        Some(ToolChoiceDirective::Hard(ToolChoice::Function { name })) => {
            json!({ "type": "function", "name": name })
        }
        Some(ToolChoiceDirective::Soft(_)) => json!("auto"),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// SSE 解析
// ──────────────────────────────────────────────────────────────────────────────

/// 单个工具调用的累积状态（Responses 以 `item_id` 标识条目，`call_id` 才是回填 id）。
#[derive(Debug, Default, Clone)]
struct ToolCallAccum {
    /// `function_call` 的 `call_id`（回填 tool 消息用）。
    call_id: Option<String>,
    name: Option<String>,
    args: String,
    started: bool,
}

/// 解析 Responses SSE 流为 [`AssistantEvent`]。
fn parse_sse_stream(resp: reqwest::Response, model_id: String) -> AssistantEventStream {
    let mut bytes_stream = resp.bytes_stream();
    let s = stream! {
        let mut buf: Vec<u8> = Vec::new();
        let mut text_buf = String::new();
        // item_id → 工具调用累积器（`response.output_item.added` 建立映射）。
        let mut item_to_call: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut tool_calls: Vec<ToolCallAccum> = Vec::new();
        let mut usage = Usage::default();
        let mut status: Option<String> = None;
        let mut completed = false;

        loop {
            let chunk_res = match tokio::time::timeout(crate::STREAM_IDLE_TIMEOUT, bytes_stream.next()).await {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(_) => {
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
                    yield AssistantEvent::Error(LlmError::StreamInterrupted(e.to_string()));
                    break;
                }
            };
            buf.extend_from_slice(chunk.as_ref());
            while let Some(line_bytes) = crate::drain_line(&mut buf) {
                let line = String::from_utf8_lossy(&line_bytes).trim().to_string();
                if line.is_empty() {
                    continue;
                }
                // Responses 同时发 `event: <name>` 与 `data: {...}`；事件名在 data 内也有 `type`。
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    continue; // 终止以 response.completed / response.failed 为准
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
                let event_type = value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                match event_type.as_str() {
                    "response.output_text.delta" => {
                        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                            if !delta.is_empty() {
                                text_buf.push_str(delta);
                                yield AssistantEvent::TextDelta(delta.to_string());
                            }
                        }
                    }
                    "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                            if !delta.is_empty() {
                                yield AssistantEvent::ThinkingDelta(delta.to_string());
                            }
                        }
                    }
                    "response.output_item.added" => {
                        let item = value.get("item").cloned().unwrap_or(Value::Null);
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                            let mut acc = ToolCallAccum {
                                call_id: item.get("call_id").and_then(Value::as_str).map(str::to_string),
                                name: item.get("name").and_then(Value::as_str).map(str::to_string),
                                args: String::new(),
                                started: false,
                            };
                            // 非流式分片（部分网关一次性给全参数）：直接落参数。
                            if let Some(args) = item.get("arguments").and_then(Value::as_str) {
                                if !args.is_empty() {
                                    acc.args.push_str(args);
                                }
                            }
                            let call_id = acc.call_id.clone().unwrap_or_else(|| item_id.clone());
                            let name = acc.name.clone().unwrap_or_default();
                            acc.started = true;
                            tool_calls.push(acc);
                            item_to_call.insert(item_id, call_id.clone());
                            yield AssistantEvent::ToolCallStart { id: call_id, name };
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        let item_id = value.get("item_id").and_then(Value::as_str).unwrap_or_default();
                        let delta = value.get("delta").and_then(Value::as_str).unwrap_or_default();
                        let call_id = item_to_call
                            .get(item_id)
                            .cloned()
                            .or_else(|| {
                                // 未见 added（网关省略该事件）：退回 item_id 作为 id。
                                if item_id.is_empty() { None } else { Some(item_id.to_string()) }
                            });
                        if let Some(id) = call_id {
                            if let Some(acc) = tool_calls.iter_mut().find(|c| {
                                c.call_id.as_deref() == Some(id.as_str())
                            }) {
                                acc.args.push_str(delta);
                            } else {
                                tool_calls.push(ToolCallAccum {
                                    call_id: Some(id.clone()),
                                    name: None,
                                    args: delta.to_string(),
                                    started: true,
                                });
                            }
                            if !delta.is_empty() {
                                yield AssistantEvent::ToolCallDelta { id, partial_json: delta.to_string() };
                            }
                        }
                    }
                    "response.output_item.done" => {
                        let item = value.get("item").cloned().unwrap_or(Value::Null);
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                            let call_id = item
                                .get("call_id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .or_else(|| item_to_call.get(item_id).cloned());
                            if let Some(call_id) = call_id {
                                if let Some(acc) = tool_calls
                                    .iter_mut()
                                    .find(|c| c.call_id.as_deref() == Some(call_id.as_str()))
                                {
                                    // 权威参数以 done 事件为准（覆盖增量拼接的结果）。
                                    if let Some(args) = item.get("arguments").and_then(Value::as_str) {
                                        acc.args = args.to_string();
                                    }
                                    if acc.name.is_none() {
                                        acc.name = item.get("name").and_then(Value::as_str).map(str::to_string);
                                    }
                                }
                                yield AssistantEvent::ToolCallEnd { id: call_id };
                            }
                        }
                    }
                    "response.completed" | "response.incomplete" => {
                        let response = value.get("response").cloned().unwrap_or(Value::Null);
                        status = response.get("status").and_then(Value::as_str).map(str::to_string);
                        if let Some(u) = response.get("usage") {
                            usage.input_tokens = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                            usage.output_tokens = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                            usage.cache_read_tokens = u
                                .get("input_tokens_details")
                                .and_then(|d| d.get("cached_tokens"))
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            yield AssistantEvent::Usage(usage.clone());
                        }
                        // incomplete：以 `incomplete_details.reason == "max_output_tokens"` 判长度截断。
                        if event_type == "response.incomplete" {
                            status = Some(
                                response
                                    .get("incomplete_details")
                                    .and_then(|d| d.get("reason"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("incomplete")
                                    .to_string(),
                            );
                        }
                        completed = true;
                    }
                    "response.failed" => {
                        let response = value.get("response").cloned().unwrap_or(Value::Null);
                        let msg = response
                            .get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("response.failed");
                        yield AssistantEvent::Error(LlmError::Http { status: 200, body: msg.to_string() });
                        completed = true;
                    }
                    "error" => {
                        let msg = value
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("responses stream error");
                        yield AssistantEvent::Error(LlmError::Http { status: 200, body: msg.to_string() });
                        completed = true;
                    }
                    _ => {}
                }
            }
            if completed {
                break;
            }
            if crate::line_buffer_too_long(&buf) {
                yield AssistantEvent::Error(LlmError::StreamInterrupted(
                    "SSE 行超过最大长度上限".into(),
                ));
                break;
            }
        }

        yield AssistantEvent::MessageEnd(build_message(&model_id, &text_buf, &tool_calls, status.as_deref(), &usage));
    };
    Box::pin(s) as Pin<Box<dyn futures::Stream<Item = AssistantEvent> + Send>>
}

/// 组装终态消息：文本块 + 工具调用块 + stop_reason 映射。
fn build_message(
    model_id: &str,
    text_buf: &str,
    tool_calls: &[ToolCallAccum],
    status: Option<&str>,
    usage: &Usage,
) -> agent_core::AssistantMessage {
    let mut content = Vec::new();
    if !text_buf.is_empty() {
        content.push(ContentBlock::Text {
            text: text_buf.to_string(),
        });
    }
    for (i, tc) in tool_calls.iter().enumerate() {
        let id = tc.call_id.clone().unwrap_or_else(|| format!("call_{i}"));
        let name = tc.name.clone().unwrap_or_default();
        let arguments = if tc.args.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&tc.args).unwrap_or_else(|_| Value::String(tc.args.clone()))
        };
        content.push(ContentBlock::ToolCall {
            id,
            name,
            arguments,
            signature: None,
        });
    }
    let has_tools = tool_calls
        .iter()
        .any(|t| t.call_id.is_some() || !t.args.is_empty());
    let (stop_reason, stop_details) = match status {
        Some("incomplete" | "max_output_tokens") => (Some(StopReason::Length), None),
        Some("failed") => (
            Some(StopReason::Error),
            Some(agent_core::StopDetails::new("sensitive")),
        ),
        _ if has_tools => (Some(StopReason::ToolUse), None),
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ContentBlock, ToolSpec};

    fn req() -> CompletionRequest {
        CompletionRequest {
            model: agent_core::Model::with_defaults("gpt-5", "openai", Api::OpenAiResponses),
            system: vec!["sys".into()],
            messages: vec![ProviderMessage::User {
                content: vec![UserContent::Text { text: "hi".into() }],
            }],
            tools: vec![ToolSpec::new(
                "read_file",
                "read",
                json!({"type": "object"}),
            )],
            tool_choice: Some(ToolChoiceDirective::Hard(ToolChoice::Auto)),
            max_tokens: 64,
            temperature: Some(0.2),
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        }
    }

    /// H14 回归：Responses 必须由本适配器承载（不再落 `Unsupported`）。
    #[test]
    fn supports_responses_wire() {
        assert_eq!(SUPPORTED, &[Api::OpenAiResponses]);
    }

    #[test]
    fn body_uses_responses_shape_not_chat_shape() {
        let body = build_body(&req(), false);
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_output_tokens"], 64);
        assert_eq!(body["instructions"], "sys");
        // 关键差异：无 chat 的 `messages`，改为 `input` 条目数组。
        assert!(body.get("messages").is_none());
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        // 工具是扁平结构（非 chat 的 tools[].function.*）。
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn body_serializes_history_and_tool_results() {
        let mut r = req();
        r.messages = vec![
            ProviderMessage::Assistant {
                content: vec![
                    ContentBlock::Text {
                        text: "let me look".into(),
                    },
                    ContentBlock::ToolCall {
                        id: "call_1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "a.rs"}),
                        signature: None,
                    },
                ],
            },
            ProviderMessage::Tool {
                tool_call_id: "call_1".into(),
                content: "content".into(),
                is_error: true,
                images: vec![],
            },
        ];
        let body = build_body(&r, false);
        let input = body["input"].as_array().unwrap();
        // 助手文本 → output_text 消息；工具调用 → 顶层 function_call 条目。
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[0]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert!(input[2]["output"].as_str().unwrap().contains("[error]"));
    }

    #[test]
    fn codex_backend_adds_store_and_include() {
        let plain = build_body(&req(), false);
        assert!(plain.get("store").is_none());
        let codex = build_body(&req(), true);
        assert_eq!(codex["store"], false);
        assert_eq!(codex["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn codex_backend_detection() {
        assert!(is_codex_backend("https://chatgpt.com/backend-api/codex"));
        assert!(!is_codex_backend("https://api.openai.com/v1"));
    }

    #[test]
    fn message_end_maps_tool_calls_and_stop_reason() {
        let calls = vec![ToolCallAccum {
            call_id: Some("call_9".into()),
            name: Some("read_file".into()),
            args: r#"{"path":"a.rs"}"#.into(),
            started: true,
        }];
        let msg = build_message(
            "gpt-5",
            "done",
            &calls,
            Some("completed"),
            &Usage::default(),
        );
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(&msg.content[1], ContentBlock::ToolCall { id, .. } if id == "call_9"));
        assert_eq!(msg.stop_reason, Some(StopReason::ToolUse));

        let text_only = build_message("gpt-5", "hi", &[], Some("completed"), &Usage::default());
        assert_eq!(text_only.stop_reason, Some(StopReason::Stop));

        let truncated = build_message(
            "gpt-5",
            "hi",
            &[],
            Some("max_output_tokens"),
            &Usage::default(),
        );
        assert_eq!(truncated.stop_reason, Some(StopReason::Length));
    }

    #[test]
    fn thinking_budget_maps_to_medium_effort() {
        let mut r = req();
        r.thinking = Some(agent_core::ThinkingConfig::new(4096));
        assert_eq!(reasoning_effort(&r), Some("low"));
        r.thinking = Some(agent_core::ThinkingConfig::new(16_000));
        assert_eq!(reasoning_effort(&r), Some("medium"));
        r.thinking = Some(agent_core::ThinkingConfig::new(40_000));
        assert_eq!(reasoning_effort(&r), Some("high"));
        r.thinking = None;
        assert_eq!(reasoning_effort(&r), None);
    }

    #[test]
    fn tool_choice_maps_to_responses_vocabulary() {
        assert_eq!(
            tool_choice(Some(&ToolChoiceDirective::Hard(ToolChoice::Required))),
            json!("required")
        );
        assert_eq!(
            tool_choice(Some(&ToolChoiceDirective::Hard(ToolChoice::None))),
            json!("none")
        );
        assert_eq!(
            tool_choice(Some(&ToolChoiceDirective::Hard(ToolChoice::Function {
                name: "read_file".into()
            }))),
            json!({"type": "function", "name": "read_file"})
        );
        assert_eq!(tool_choice(None), json!("auto"));
    }
}
