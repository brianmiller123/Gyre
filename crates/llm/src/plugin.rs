//! 插件零侵入注册：通过 `inventory::submit!` 自荐 Provider，无需修改中央清单。
//!
//! 新增 Provider：① `impl LlmProvider`；② 在本文件 `inventory::submit!{ LlmProviderPlugin::new(|client| Box::new(MyAdapter::new(client))) }`；
//! 启动时 [`collect_providers`] 自动遍历装配，cli/server 装配零改动。

use agent_core::LlmProvider;

/// Provider 工厂插件（接收共享的 `reqwest::Client`）。
pub struct LlmProviderPlugin {
    /// 工厂函数。
    pub factory: fn(reqwest::Client) -> Box<dyn LlmProvider>,
}

impl LlmProviderPlugin {
    /// 构造插件（用于 `inventory::submit!`）。
    #[must_use]
    pub const fn new(factory: fn(reqwest::Client) -> Box<dyn LlmProvider>) -> Self {
        Self { factory }
    }
}

impl inventory::Collect for LlmProviderPlugin {
    #[inline]
    fn registry() -> &'static inventory::Registry {
        static REGISTRY: inventory::Registry = inventory::Registry::new();
        &REGISTRY
    }
}

/// 收集所有自荐的 Provider 插件（用共享 `client` 构造），返回可注册的 trait object 列表。
pub fn collect_providers(client: reqwest::Client) -> Vec<Box<dyn LlmProvider>> {
    inventory::iter::<LlmProviderPlugin>()
        .map(|p| (p.factory)(client.clone()))
        .collect()
}

// 内置 Provider 自荐注册（新增 Provider 仅需在此 submit!，cli/server 装配零改动）。
inventory::submit! {
    LlmProviderPlugin::new(|client| Box::new(crate::OpenAiCompletionsAdapter::new(client)))
}
inventory::submit! {
    LlmProviderPlugin::new(|client| Box::new(crate::AnthropicMessagesAdapter::new(client)))
}
inventory::submit! {
    LlmProviderPlugin::new(|client| Box::new(crate::DeepSeekProvider::new(client)))
}
inventory::submit! {
    LlmProviderPlugin::new(|client| Box::new(crate::GlmProvider::new(client)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{Api, AssistantEventStream, CompletionRequest, LlmError, ProviderCallContext};

    struct DummyProvider;
    #[async_trait::async_trait]
    impl LlmProvider for DummyProvider {
        fn id(&self) -> &'static str {
            "dummy-test"
        }
        fn supports(&self) -> &[Api] {
            &[]
        }
        async fn stream(
            &self,
            _r: CompletionRequest,
            _c: &ProviderCallContext,
        ) -> Result<AssistantEventStream, LlmError> {
            Err(LlmError::Unsupported("dummy".into()))
        }
    }

    inventory::submit! {
        LlmProviderPlugin::new(|_client| Box::new(DummyProvider))
    }

    #[test]
    fn collects_self_submitted_and_builtin_providers() {
        let providers = collect_providers(reqwest::Client::new());
        assert!(providers.iter().any(|p| p.id() == "dummy-test"));
        assert!(providers.iter().any(|p| p.id() == "openai-completions"));
        assert!(providers.iter().any(|p| p.id() == "anthropic-messages"));
        assert!(providers.iter().any(|p| p.id() == "deepseek"));
        assert!(providers.iter().any(|p| p.id() == "glm"));
    }
}
