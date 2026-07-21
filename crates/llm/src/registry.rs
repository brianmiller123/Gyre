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
        self.stream_simple(request, ctx).await
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
}
