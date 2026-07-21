//! # 自适应思考难度分类器
//!
//! 移植 oh-my-pi [`auto-thinking/classifier.ts`]：用 tiny/smol 模型把用户 prompt 分到
//! [`Effort`] 档（low/medium/high/xhigh），映射为 `budget_tokens` 并钳到模型范围。
//! 实现 [`ThinkingClassifier`] trait，供 [`ThinkingPolicy::Auto`] 经 run_loop 每轮解析。
//!
//! 设计要点（对齐 oh-my-pi）：
//! - **单次 8-token 输出**：分类是单词任务，预算极小（`ANSWER_MAX_TOKENS=8`）。
//! - **输入截断**：超长 prompt 取 head 4000 + tail 2000（`MAX_INPUT_CHARS=6000`），避免
//!   浪费 token 且降低准确度（截断由 `ThinkingPolicy::resolve` 完成，本模块收到的已是截断后文本）。
//! - **失败回退**：任何错误（无模型、网络、不可解析、abort）→ 返回 `None`，调用方用 fallback，
//!   不阻断 turn。
//!
//! [`auto-thinking/classifier.ts`]: https://github.com/can1357/oh-my-pi/blob/master/packages/coding-agent/src/auto-thinking/classifier.ts

use std::sync::Arc;

use agent_core::{
    AssistantEvent, CompletionRequest, Effort, LlmProvider, Model, ProviderCallContext,
    ProviderMessage, ThinkingClassifier, UserContent,
};

/// 分类器输出的最大 token（单词回答，移植 oh-my-pi `ANSWER_MAX_TOKENS`）。
const ANSWER_MAX_TOKENS: usize = 8;

/// 难度分类 system prompt（移植 oh-my-pi `auto-thinking-difficulty.md`，精简为四档）。
const DIFFICULTY_SYSTEM_PROMPT: &str = "\
你是一个编程任务难度分类器。判断给定任务的难度，仅输出一个词：low、medium、high 或 xhigh。

判定标准：
- low：简单问答、列表、查找、单步无歧义操作。
- medium：常规编码、解释、单文件改动、遵循明确规范的重写。
- high：调试、重构、多文件改动、需要设计与权衡。
- xhigh：跨文件架构改动、复杂推理、性能/并发/正确性深度分析。

只输出一个词（low / medium / high / xhigh），不要解释、不要标点。";

/// 基于 [`LlmProvider`] 的思考难度分类器。
///
/// 持有一个 tiny/smol [`Model`]（如 gpt-4o-mini / glm-flash）与配套 [`ProviderCallContext`]，
/// 经单次 8-token 补全分类 prompt 难度。无可用模型（`provider` 为 `UnconfiguredProvider`）
/// 或调用/解析失败时返回 `None`，由 [`ThinkingPolicy::Auto`](agent_core::ThinkingPolicy::Auto) 回退 fallback。
pub struct LlmThinkingClassifier {
    provider: Arc<dyn LlmProvider>,
    classifier_model: Model,
    ctx: ProviderCallContext,
}

impl LlmThinkingClassifier {
    /// 构造分类器。
    ///
    /// `classifier_model` 应为廉价 tiny 模型（低成本、低延迟）；`ctx` 携带鉴权与 base URL。
    #[must_use]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        classifier_model: Model,
        ctx: ProviderCallContext,
    ) -> Self {
        Self {
            provider,
            classifier_model,
            ctx,
        }
    }
}

#[async_trait::async_trait]
impl ThinkingClassifier for LlmThinkingClassifier {
    async fn classify(&self, prompt: &str, _model: &Model) -> Option<Effort> {
        // 不发工具（纯文本分类）、关闭思考（分类无需 reasoning）、温度 0（确定性）。
        let req = CompletionRequest {
            model: self.classifier_model.clone(),
            system: vec![DIFFICULTY_SYSTEM_PROMPT.to_string()],
            messages: vec![ProviderMessage::User {
                content: vec![UserContent::Text {
                    text: prompt.to_string(),
                }],
            }],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: ANSWER_MAX_TOKENS,
            temperature: Some(0.0),
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        };
        let mut stream = self.provider.stream(req, &self.ctx).await.ok()?;
        // 仅取 MessageEnd 的权威完整文本（TextDelta 为增量，二者叠加会重复；分类只需最终单词）。
        while let Some(ev) = futures::StreamExt::next(&mut stream).await {
            match ev {
                AssistantEvent::MessageEnd(msg) => return Effort::parse(&msg.text()),
                AssistantEvent::Error(_) => return None,
                _ => {}
            }
        }
        None // 流未到达 MessageEnd 即结束（异常断开）→ 分类失败，回退。
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AssistantMessage, LlmError, Usage};
    use futures::stream::StreamExt;

    /// 桩 Provider：流式下发预设文本，用于测试分类器解析。
    struct StubClassifierProvider {
        answer: &'static str,
    }

    #[async_trait::async_trait]
    impl LlmProvider for StubClassifierProvider {
        fn id(&self) -> &'static str {
            "stub-classifier"
        }
        fn supports(&self) -> &[agent_core::model::Api] {
            &[]
        }
        async fn stream(
            &self,
            _request: CompletionRequest,
            _ctx: &ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, LlmError> {
            let answer = self.answer;
            let evs: Vec<AssistantEvent> = vec![
                AssistantEvent::TextDelta(answer.to_string()),
                AssistantEvent::MessageEnd(AssistantMessage {
                    content: vec![agent_core::ContentBlock::Text {
                        text: answer.to_string(),
                    }],
                    usage: Usage::default(),
                    model: "stub".into(),
                    stop_reason: Some(agent_core::StopReason::Stop),
                    stop_details: None,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    fn tiny_model() -> Model {
        Model::with_defaults("tiny", "tiny", agent_core::Api::OpenAiCompletions)
    }

    #[tokio::test]
    async fn classify_parses_effort_from_provider_text() {
        for (answer, expected) in [
            ("low", Effort::Low),
            ("Medium", Effort::Medium),
            ("  high\n", Effort::High),
            ("xhigh", Effort::XHigh),
        ] {
            let clf = LlmThinkingClassifier::new(
                Arc::new(StubClassifierProvider { answer }),
                tiny_model(),
                ProviderCallContext::default(),
            );
            let target = tiny_model();
            assert_eq!(clf.classify("some prompt", &target).await, Some(expected));
        }
    }

    #[tokio::test]
    async fn classify_returns_none_on_unparsable_output() {
        let clf = LlmThinkingClassifier::new(
            Arc::new(StubClassifierProvider {
                answer: "也许吧"
            }),
            tiny_model(),
            ProviderCallContext::default(),
        );
        let target = tiny_model();
        assert_eq!(clf.classify("prompt", &target).await, None);
    }

    /// 桩 Provider：stream 返回错误 → 分类器返回 None（不阻断）。
    struct ErrorProvider;
    #[async_trait::async_trait]
    impl LlmProvider for ErrorProvider {
        fn id(&self) -> &'static str {
            "err"
        }
        fn supports(&self) -> &[agent_core::model::Api] {
            &[]
        }
        async fn stream(
            &self,
            _request: CompletionRequest,
            _ctx: &ProviderCallContext,
        ) -> Result<agent_core::AssistantEventStream, LlmError> {
            Err(LlmError::Transport("boom".into()))
        }
    }

    #[tokio::test]
    async fn classify_returns_none_on_provider_error() {
        let clf = LlmThinkingClassifier::new(
            Arc::new(ErrorProvider),
            tiny_model(),
            ProviderCallContext::default(),
        );
        let target = tiny_model();
        assert_eq!(clf.classify("prompt", &target).await, None);
    }
}
