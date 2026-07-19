//! [`InbandProvider`]：用 in-band 方言包装任意 [`LlmProvider`]，把工具规格移入
//! system prompt、从模型文本输出解析工具调用——对 function-calling 不稳的模型
//! （GLM / DeepSeek 等）提供稳定的「提示词 + 文本协议」工具能力。
//!
//! 工作流：
//! 1. 请求侧：若 `tools` 非空，渲染工具段追加到 `system`，清空 `tools`/`tool_choice`
//!    （不发原生 function-calling，避免不稳的 backend 误解析）。
//! 2. 响应侧：透传流式事件并累积文本；在 `MessageEnd` 处用方言解析工具调用，重建
//!    消息内容为「纯文本 + ToolCall 块」，并把 `stop_reason` 修正为 `ToolUse`。
//!    agent 循环据此执行工具（与原生 tool_calls 路径一致）。

use std::sync::Arc;

use agent_core::{
    Api, AssistantEvent, AssistantEventStream, CompletionRequest, ContentBlock, LlmError, LlmProvider,
    ProviderCallContext, StopReason,
};
use async_trait::async_trait;
use futures::StreamExt;

use crate::dialect::Dialect;

/// In-band 工具调用 Provider 包装。
pub struct InbandProvider {
    inner: Arc<dyn LlmProvider>,
    dialect: Dialect,
}

impl InbandProvider {
    /// 用 `dialect` 包装 `inner`。
    #[must_use]
    pub fn new(inner: Arc<dyn LlmProvider>, dialect: Dialect) -> Self {
        Self { inner, dialect }
    }
}

/// 条件包装：`flag` 非空且非 `"0"`/`"false"` 时，用 [`Dialect::Xml`] 把 `provider` 包成
/// in-band；否则原样返回。供 cli/server 用环境变量（如 `GYRE_INBAND_TOOLS=1`）opt-in，
/// 对 function-calling 不稳的模型启用「提示词 + 文本协议」工具调用。
#[must_use]
pub fn wrap_inband_if(
    provider: Arc<dyn LlmProvider>,
    flag: Option<&str>,
) -> Arc<dyn LlmProvider> {
    match flag.map(str::trim) {
        Some(v) if !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false") => {
            Arc::new(InbandProvider::new(provider, Dialect::Xml))
        }
        _ => provider,
    }
}

#[async_trait]
impl LlmProvider for InbandProvider {
    fn id(&self) -> &'static str {
        "inband"
    }
    fn supports(&self) -> &[Api] {
        // 透传底层能力，便于 registry/路由复用。
        self.inner.supports()
    }

    async fn stream(
        &self,
        mut req: CompletionRequest,
        ctx: &ProviderCallContext,
    ) -> Result<AssistantEventStream, LlmError> {
        if !req.tools.is_empty() {
            let prompt = self.dialect.render_tools(&req.tools);
            req.system.push(prompt);
            req.tools.clear();
            req.tool_choice = None;
        }
        let inner = self.inner.stream(req, ctx).await?;
        Ok(Box::pin(async_stream::stream! {
            // 增量解析：抑制 <tool_call>...</tool_call> 标记本身，仅下发普通文本；
            // 工具调用内联 JSON 在闭合后收集，于 MessageEnd 重建为 ToolCall 块。
            let mut parser = crate::dialect::XmlToolStreamParser::new();
            let mut tool_jsons: Vec<String> = Vec::new();
            let mut cleaned = String::new();
            let mut s = inner;
            while let Some(ev) = s.next().await {
                match ev {
                    AssistantEvent::TextDelta(d) => {
                        let (text, completed) = parser.feed(&d);
                        tool_jsons.extend(completed);
                        cleaned.push_str(&text);
                        if !text.is_empty() {
                            yield AssistantEvent::TextDelta(text);
                        }
                    }
                    AssistantEvent::MessageEnd(mut msg) => {
                        // 收尾：冲刷被保留的尾巴文本（跨 chunk 的 OPEN 前缀 / 工具后文本）。
                        let (tail, _) = parser.finish();
                        if !tail.is_empty() {
                            cleaned.push_str(&tail);
                            yield AssistantEvent::TextDelta(tail);
                        }
                        let (calls, cleaned_final) = crate::dialect::finalize_tool_calls(
                            std::mem::take(&mut cleaned),
                            std::mem::take(&mut tool_jsons),
                        );
                        // 重建内容：保留非文本块（thinking 等），文本块统一用 cleaned_final。
                        let mut new_content: Vec<ContentBlock> = Vec::new();
                        let mut text_pushed = false;
                        for b in msg.content.drain(..) {
                            if matches!(b, ContentBlock::Text { .. }) {
                                if !text_pushed {
                                    new_content.push(ContentBlock::Text { text: cleaned_final.clone() });
                                    text_pushed = true;
                                }
                            } else {
                                new_content.push(b);
                            }
                        }
                        if !text_pushed && !cleaned_final.trim().is_empty() {
                            new_content.push(ContentBlock::Text { text: cleaned_final.clone() });
                        }
                        let has_call = !calls.is_empty();
                        new_content.extend(calls);
                        msg.content = new_content;
                        if has_call {
                            // 解析出工具调用 → 标记 ToolUse，供 agent 循环走工具执行路径。
                            msg.stop_reason = Some(StopReason::ToolUse);
                        }
                        yield AssistantEvent::MessageEnd(msg);
                    }
                    other => yield other,
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AssistantMessage, Model, ToolSpec, Usage};
    use futures::stream::StreamExt;
    use serde_json::json;

    /// 桩 Provider：忽略请求，吐出固定事件序列（含一段带工具调用块的文本）。
    struct FakeProvider;

    #[async_trait]
    impl LlmProvider for FakeProvider {
        fn id(&self) -> &'static str { "fake" }
        fn supports(&self) -> &[Api] { &[] }
        async fn stream(
            &self,
            _req: CompletionRequest,
            _ctx: &ProviderCallContext,
        ) -> Result<AssistantEventStream, LlmError> {
            let text = "我先读取文件。\n<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}</tool_call>\n";
            let msg = AssistantMessage {
                content: vec![ContentBlock::Text { text: text.to_string() }],
                usage: Usage::default(),
                model: "fake".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![
                AssistantEvent::TextDelta(text.to_string()),
                AssistantEvent::MessageEnd(msg),
            ])))
        }
    }

    fn req_with_tools() -> CompletionRequest {
        CompletionRequest {
            model: Model::with_defaults("fake", "fake", Api::OpenAiCompletions),
            system: vec![],
            messages: vec![],
            tools: vec![ToolSpec::new("read_file", "read", json!({}))],
            tool_choice: None,
            max_tokens: 16,
            temperature: None,
            thinking: None,
            cache_key: None,
        }
    }

    #[tokio::test]
    async fn inband_extracts_tool_call_from_text() {
        let provider = InbandProvider::new(Arc::new(FakeProvider), Dialect::Xml);
        let mut s = provider
            .stream(req_with_tools(), &ProviderCallContext::default())
            .await
            .unwrap();
        let mut found_call = false;
        let mut found_stop_reason = false;
        while let Some(ev) = s.next().await {
            if let AssistantEvent::MessageEnd(msg) = ev {
                let has = msg.content.iter().any(|b| {
                    matches!(b, ContentBlock::ToolCall { name, .. } if name == "read_file")
                });
                found_call |= has;
                found_stop_reason |= msg.stop_reason == Some(StopReason::ToolUse);
            }
        }
        assert!(found_call, "应从文本解析出 read_file 工具调用");
        assert!(
            found_stop_reason,
            "解析出工具调用后 stop_reason 应修正为 ToolUse"
        );
    }

    #[tokio::test]
    async fn inband_no_tools_request_passes_through() {
        // 请求无 tools → 不应改写（FakeProvider 本就无原生工具，行为一致）。
        let provider = InbandProvider::new(Arc::new(FakeProvider), Dialect::Xml);
        let mut req = req_with_tools();
        req.tools.clear();
        let mut s = provider.stream(req, &ProviderCallContext::default()).await.unwrap();
        // FakeProvider 的文本里仍含 <tool_call>，故仍会被解析——这里只验证流不报错、有 MessageEnd。
        let mut saw_end = false;
        while let Some(ev) = s.next().await {
            if matches!(ev, AssistantEvent::MessageEnd(_)) {
                saw_end = true;
            }
        }
        assert!(saw_end);
    }

    /// 桩 Provider：把含工具调用的文本切成多 chunk（`<tool_call>`/`</tool_call>` 跨 chunk），
    /// 验证 InbandProvider 增量解析时不泄露标记、仍能正确提取工具调用。
    struct ChunkedFakeProvider;

    #[async_trait]
    impl LlmProvider for ChunkedFakeProvider {
        fn id(&self) -> &'static str { "chunked" }
        fn supports(&self) -> &[Api] { &[] }
        async fn stream(
            &self,
            _req: CompletionRequest,
            _ctx: &ProviderCallContext,
        ) -> Result<AssistantEventStream, LlmError> {
            let chunks = [
                "reading\n<tool",
                "_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}</tool",
                "_call>\ndone",
            ];
            let mut evs: Vec<AssistantEvent> = chunks
                .into_iter()
                .map(|c| AssistantEvent::TextDelta(c.to_string()))
                .collect();
            evs.push(AssistantEvent::MessageEnd(AssistantMessage {
                content: vec![ContentBlock::Text { text: String::new() }],
                usage: Usage::default(),
                model: "chunked".into(),
                stop_reason: Some(StopReason::Stop),
                stop_details: None,
            }));
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    #[tokio::test]
    async fn inband_suppresses_markers_across_chunks() {
        let provider = InbandProvider::new(Arc::new(ChunkedFakeProvider), Dialect::Xml);
        let mut s = provider
            .stream(req_with_tools(), &ProviderCallContext::default())
            .await
            .unwrap();
        let mut streamed = String::new();
        let mut found_call = false;
        while let Some(ev) = s.next().await {
            match ev {
                AssistantEvent::TextDelta(d) => streamed.push_str(&d),
                AssistantEvent::MessageEnd(msg) => {
                    found_call = msg.content.iter().any(|b| {
                        matches!(b, ContentBlock::ToolCall { name, .. } if name == "read_file")
                    });
                }
                _ => {}
            }
        }
        assert!(!streamed.contains("<tool_call"), "不应泄露开标记: {streamed:?}");
        assert!(!streamed.contains("</tool_call"), "不应泄露闭标记: {streamed:?}");
        assert!(
            !streamed.contains("read_file"),
            "不应泄露工具 JSON 内联: {streamed:?}"
        );
        assert!(
            streamed.contains("reading") && streamed.contains("done"),
            "普通文本应保留: {streamed:?}"
        );
        assert!(found_call, "应从跨 chunk 的标记中解析出 read_file 调用");
    }
}
