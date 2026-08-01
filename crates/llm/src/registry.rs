//! [`ProviderRegistry`] —— 单一分发入口，按 [`Api`](agent_core::Api) 路由到适配器。

use agent_core::{
    Api, AssistantEventStream, CompletionRequest, LlmError, LlmProvider, ProviderCallContext,
};

/// Provider 注册表：按模型 `api` 线协议族路由。
pub struct ProviderRegistry {
    adapters: Vec<Box<dyn LlmProvider>>,
}

impl ProviderRegistry {
    /// 空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self {
            adapters: Vec::new(),
        }
    }

    /// 注册一个适配器。
    pub fn register(&mut self, adapter: Box<dyn LlmProvider>) {
        self.adapters.push(adapter);
    }

    /// 注册并返回自身（构建器风格）。
    #[must_use]
    pub fn with(mut self, adapter: Box<dyn LlmProvider>) -> Self {
        self.register(adapter);
        self
    }

    /// 按 `api` 路由到首个支持的适配器。
    #[must_use]
    pub fn route(&self, api: Api) -> Option<&dyn LlmProvider> {
        self.adapters
            .iter()
            .find(|a| a.supports().contains(&api))
            .map(Box::as_ref)
    }

    /// 按 `api` 路由到**全部**支持该线协议族的适配器（fallback 链顺序 = 注册顺序）。
    #[must_use]
    pub fn route_all(&self, api: Api) -> Vec<&dyn LlmProvider> {
        self.adapters
            .iter()
            .filter(|a| a.supports().contains(&api))
            .map(Box::as_ref)
            .collect()
    }

    /// 带 fallback 的单一分发入口：依序尝试全部支持该 `api` 的适配器，
    /// 首个成功建立流者胜出；可重试错误（网络/5xx/429/鉴权）换下一适配器，
    /// 其余错误立即上抛；全部失败返回汇总错误（含尝试数与最后错误）。
    /// 同一适配器集合可按需注册多份（各带不同 key env 的适配器构造即凭证轮换）。
    ///
    /// # Errors
    /// 无适配器支持该 `api`、或全部适配器失败时返回 [`LlmError`]。
    pub async fn stream_fallback(
        &self,
        request: CompletionRequest,
        ctx: &ProviderCallContext,
    ) -> Result<AssistantEventStream, LlmError> {
        let api = request.model.api;
        let adapters = self.route_all(api);
        if adapters.is_empty() {
            return Err(LlmError::Unsupported(format!("无支持 {api} 的 Provider")));
        }
        let mut last_err: Option<LlmError> = None;
        for (i, adapter) in adapters.iter().enumerate() {
            match adapter.stream(request.clone(), ctx).await {
                Ok(stream) => {
                    if i > 0 {
                        tracing::info!(from = i, "provider fallback 成功（第 {} 个适配器）", i + 1);
                    }
                    return Ok(stream);
                }
                Err(e) => {
                    tracing::warn!(adapter = i, error = %e, "provider 尝试失败");
                    let fallbackable = e.is_fallbackable();
                    last_err = Some(e);
                    if !fallbackable {
                        return Err(last_err.unwrap());
                    }
                }
            }
        }
        Err(match last_err {
            Some(e) => LlmError::Transport(format!(
                "全部 {} 个适配器失败（最后一个: {e}）",
                adapters.len()
            )),
            None => LlmError::Unsupported(format!("无支持 {api} 的 Provider")),
        })
    }

    /// 单一分发入口（移植 oh-my-pi `streamSimple`）。
    ///
    /// # Errors
    /// 无适配器支持该 `api`，或适配器内部错误时返回 [`LlmError`]。
    pub async fn stream_simple(
        &self,
        request: CompletionRequest,
        ctx: &ProviderCallContext,
    ) -> Result<AssistantEventStream, LlmError> {
        let api = request.model.api;
        let adapter = self
            .route(api)
            .ok_or_else(|| LlmError::Unsupported(format!("无支持 {api} 的 Provider")))?;
        adapter.stream(request, ctx).await
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// ProviderRegistry 自身也实现 [`LlmProvider`]，便于上层（`crates/agent`）只依赖 trait，
/// 与具体适配器实现解耦：注入 `Arc<dyn LlmProvider>` 即可，无需直接依赖本 crate。
#[async_trait::async_trait]
impl LlmProvider for ProviderRegistry {
    fn id(&self) -> &'static str {
        "registry"
    }
    fn supports(&self) -> &[Api] {
        // 路由型 provider，自身不绑定特定 Api；实际能力取决于已注册适配器。
        &[]
    }
    async fn stream(
        &self,
        request: CompletionRequest,
        ctx: &ProviderCallContext,
    ) -> Result<AssistantEventStream, LlmError> {
        // 默认走 fallback 链（P2：Provider 路由增强）：多适配器时自动换下一家；
        // 单适配器时与 stream_simple 等价（可重试错误上抛路径一致）。
        self.stream_fallback(request, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::llm::UnconfiguredProvider;

    #[test]
    fn route_returns_none_when_empty() {
        let reg = ProviderRegistry::new();
        assert!(reg.route(Api::OpenAiCompletions).is_none());
    }

    #[tokio::test]
    async fn stream_simple_errors_without_adapter() {
        let reg = ProviderRegistry::new();
        let req = CompletionRequest {
            model: agent_core::Model::with_defaults("x", "openai", Api::OpenAiCompletions),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1,
            temperature: None,
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        };
        let err = reg
            .stream_simple(req, &ProviderCallContext::default())
            .await;
        assert!(err.is_err());
    }

    #[test]
    fn unconfigured_provider_routes_but_errors() {
        let reg = ProviderRegistry::new().with(Box::new(UnconfiguredProvider));
        assert!(reg.route(Api::OpenAiCompletions).is_none()); // supports() 为空
    }

    // ── fallback 链（P2：Provider 路由增强）──────────────────────────────

    fn req() -> CompletionRequest {
        CompletionRequest {
            model: agent_core::Model::with_defaults("x", "openai", Api::OpenAiCompletions),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1,
            temperature: None,
            thinking: None,
            cache_key: None,
            stable_prefix_len: 0,
        }
    }

    /// 桩适配器：按预定错误序列失败，最后成功返回空流。
    struct StubProvider {
        name: &'static str,
        /// 依序返回的错误（`None` = 成功）。
        errors: Vec<LlmError>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for StubProvider {
        fn id(&self) -> &'static str {
            self.name
        }
        fn supports(&self) -> &[Api] {
            &[Api::OpenAiCompletions]
        }
        async fn stream(
            &self,
            _req: CompletionRequest,
            _ctx: &ProviderCallContext,
        ) -> Result<AssistantEventStream, LlmError> {
            let i = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.errors.get(i) {
                Some(e) => Err(e.clone()),
                None => Ok(Box::pin(futures::stream::iter(vec![]))),
            }
        }
    }

    #[tokio::test]
    async fn fallback_skips_transport_error_to_next_adapter() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reg = ProviderRegistry::new()
            .with(Box::new(StubProvider {
                name: "fail",
                errors: vec![LlmError::Transport("boom".into())],
                calls: std::sync::Arc::clone(&calls),
            }))
            .with(Box::new(StubProvider {
                name: "ok",
                errors: vec![],
                calls: std::sync::Arc::clone(&calls),
            }));
        let stream = reg
            .stream_fallback(req(), &ProviderCallContext::default())
            .await
            .expect("第二适配器应成功");
        drop(stream);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2, "两个适配器都应被调用");
    }

    #[tokio::test]
    async fn fallback_http_5xx_retries_but_4xx_aborts() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reg = ProviderRegistry::new()
            .with(Box::new(StubProvider {
                name: "p1",
                errors: vec![LlmError::Http {
                    status: 500,
                    body: "oops".into(),
                }],
                calls: std::sync::Arc::clone(&calls),
            }))
            .with(Box::new(StubProvider {
                name: "p2",
                errors: vec![],
                calls: std::sync::Arc::clone(&calls),
            }));
        let ok = reg
            .stream_fallback(req(), &ProviderCallContext::default())
            .await
            .is_ok();
        assert!(ok, "5xx 应 fallback");

        // 400（客户端错误）→ 立即上抛，不尝试第二适配器。
        let calls2 = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reg2 = ProviderRegistry::new()
            .with(Box::new(StubProvider {
                name: "p1",
                errors: vec![LlmError::Http {
                    status: 400,
                    body: "bad request".into(),
                }],
                calls: std::sync::Arc::clone(&calls2),
            }))
            .with(Box::new(StubProvider {
                name: "p2",
                errors: vec![],
                calls: std::sync::Arc::clone(&calls2),
            }));
        let res = reg2
            .stream_fallback(req(), &ProviderCallContext::default())
            .await;
        let err = match res {
            Err(e) => e,
            Ok(_) => panic!("400 应立即失败"),
        };
        assert!(!err.is_fallbackable(), "400 不可 fallback: {err}");
        assert_eq!(calls2.load(std::sync::atomic::Ordering::SeqCst), 1, "400 不应尝试第二适配器");
    }

    #[tokio::test]
    async fn fallback_all_fail_returns_summary() {
        let reg = ProviderRegistry::new()
            .with(Box::new(StubProvider {
                name: "a",
                errors: vec![LlmError::Transport("net down".into())],
                calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }))
            .with(Box::new(StubProvider {
                name: "b",
                errors: vec![LlmError::Auth("denied".into())],
                calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }));
        let res = reg
            .stream_fallback(req(), &ProviderCallContext::default())
            .await;
        let err = match res {
            Err(e) => e,
            Ok(_) => panic!("应全部失败"),
        };
        assert!(err.to_string().contains("全部 2 个适配器失败"), "{err}");
    }
}
