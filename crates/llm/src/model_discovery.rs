//! 模型发现（`GET /models`）。
//!
//! 供装配层拉取远端可用模型列表：按线协议族区分端点与鉴权头——
//! - `openai-completions` / `deepseek`：`GET {base}/models`，`Authorization: Bearer <key>`；
//! - `anthropic-messages`：`GET {base}/v1/models`，`x-api-key` + `anthropic-version: 2023-06-01`。
//!
//! `base_url` 处理与各 Provider 适配器一致：尾斜杠剪除；空串回退该协议的默认源。

use std::time::Duration;

use agent_core::Api;
use serde::{Deserialize, Serialize};

/// 单次模型发现请求的整体超时。
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// 远端模型条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModel {
    /// 模型 ID（如 `gpt-4o` / `deepseek-chat` / `claude-sonnet-4-5`）。
    pub id: String,
    /// 归属方（OpenAI 兼容 `/models` 的 `owned_by`；Anthropic 不提供 → `None`）。
    #[serde(default)]
    pub owned_by: Option<String>,
}

/// 模型发现错误。
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// 该线协议族没有已实现的模型发现端点。
    #[error("线协议 {api} 不支持模型发现")]
    Unsupported {
        /// 不支持的线协议族。
        api: Api,
    },
    /// 上游返回非 2xx（信息含状态码与截断后的响应体）。
    #[error("HTTP {status}: {body}")]
    Http {
        /// HTTP 状态码。
        status: u16,
        /// 响应体（截断至 4 KiB）。
        body: String,
    },
    /// 响应体不是合法 JSON，或缺少 `data` 数组 / 条目字段类型不符。
    #[error("模型列表响应解析失败: {0}")]
    Parse(String),
    /// 网络层错误（连接失败、整体超时等）。
    #[error("模型发现请求失败: {source}")]
    Network {
        /// 底层 reqwest 错误。
        #[from]
        source: reqwest::Error,
    },
}

/// OpenAI 兼容 `/models` 条目（wire 键为 snake_case，与 RPC 面 camelCase 区分）。
#[derive(Debug, Deserialize)]
struct OpenAiModelEntry {
    id: String,
    #[serde(default)]
    owned_by: Option<String>,
}

/// `/models` 响应外壳（OpenAI 兼容与 Anthropic 均为 `{ "data": [...] }`）。
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<OpenAiModelEntry>,
}

/// 各支持线协议的默认源（与对应 Provider 适配器的缺省 base URL 一致）。
const fn default_base_url(api: Api) -> Option<&'static str> {
    match api {
        Api::OpenAiCompletions => Some("https://api.openai.com/v1"),
        Api::DeepSeek => Some("https://api.deepseek.com"),
        Api::AnthropicMessages => Some("https://api.anthropic.com"),
        _ => None,
    }
}

/// 拉取远端可用模型列表。
///
/// - `openai-completions` / `deepseek`：`GET {base}/models` + `Authorization: Bearer`；
/// - `anthropic-messages`：`GET {base}/v1/models` + `x-api-key` + `anthropic-version`；
/// - 其余线协议族：[`DiscoveryError::Unsupported`]。
///
/// `base_url` 尾斜杠剪除；空串回退对应协议默认源。整体超时 15s。
///
/// # Errors
/// 见 [`DiscoveryError`]。
pub async fn list_models(
    api: Api,
    base_url: &str,
    api_key: &str,
    http: reqwest::Client,
) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let base = if base_url.trim().is_empty() {
        default_base_url(api).ok_or(DiscoveryError::Unsupported { api })?
    } else {
        base_url.trim_end_matches('/')
    };

    let req = match api {
        Api::OpenAiCompletions | Api::DeepSeek => {
            http.get(format!("{base}/models")).bearer_auth(api_key)
        }
        Api::AnthropicMessages => http
            .get(format!("{base}/v1/models"))
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
        other => return Err(DiscoveryError::Unsupported { api: other }),
    };

    let resp = req.timeout(DISCOVERY_TIMEOUT).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = crate::read_error_body(resp).await;
        return Err(DiscoveryError::Http {
            status: status.as_u16(),
            body,
        });
    }

    let parsed: ModelsResponse = resp
        .json()
        .await
        .map_err(|e| DiscoveryError::Parse(format!("缺少 data 数组或条目字段类型不符: {e}")))?;
    Ok(parsed
        .data
        .into_iter()
        .map(|e| DiscoveredModel {
            id: e.id,
            owned_by: e.owned_by,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn openai_completions_lists_models_with_bearer_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "id": "gpt-4o", "owned_by": "openai" },
                    { "id": "llama-3" }
                ]
            })))
            .mount(&server)
            .await;

        // 尾斜杠剪除：base 以 `/` 结尾也应命中 `/models`（否则 wiremock 回 404）。
        let models = list_models(
            Api::OpenAiCompletions,
            &format!("{}/", server.uri()),
            "sk-test",
            client(),
        )
        .await
        .expect("发现应成功");

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-4o");
        assert_eq!(models[0].owned_by.as_deref(), Some("openai"));
        assert_eq!(models[1].id, "llama-3");
        assert!(models[1].owned_by.is_none(), "缺省 owned_by → None");
    }

    #[tokio::test]
    async fn deepseek_lists_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "id": "deepseek-chat", "owned_by": "deepseek" }]
            })))
            .mount(&server)
            .await;

        let models = list_models(Api::DeepSeek, &server.uri(), "k", client())
            .await
            .expect("发现应成功");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "deepseek-chat");
        assert_eq!(models[0].owned_by.as_deref(), Some("deepseek"));
    }

    #[tokio::test]
    async fn anthropic_uses_v1_models_and_version_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "sk-ant"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "type": "model", "id": "claude-sonnet-4-5", "display_name": "Claude" }
                ]
            })))
            .mount(&server)
            .await;

        let models = list_models(Api::AnthropicMessages, &server.uri(), "sk-ant", client())
            .await
            .expect("发现应成功");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-sonnet-4-5");
        assert!(models[0].owned_by.is_none(), "Anthropic 无 owned_by → None");
    }

    #[tokio::test]
    async fn http_error_carries_status_code() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"bad key"}"#))
            .mount(&server)
            .await;

        let err = list_models(Api::OpenAiCompletions, &server.uri(), "bad", client())
            .await
            .expect_err("401 应报错");
        let msg = err.to_string();
        assert!(msg.contains("401"), "错误信息应含状态码: {msg}");
        match err {
            DiscoveryError::Http { status, body } => {
                assert_eq!(status, 401);
                assert!(body.contains("bad key"));
            }
            other => panic!("应为 Http 错误，实际: {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_bodies_are_parse_errors() {
        // 非 JSON 垃圾体。
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json{{"))
            .mount(&server)
            .await;
        let err = list_models(Api::OpenAiCompletions, &server.uri(), "k", client())
            .await
            .expect_err("垃圾体应报解析错误");
        assert!(matches!(err, DiscoveryError::Parse(_)), "实际: {err:?}");

        // data 字段类型不符（字符串而非数组）。
        let server2 = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": "nope" })),
            )
            .mount(&server2)
            .await;
        let err = list_models(Api::DeepSeek, &server2.uri(), "k", client())
            .await
            .expect_err("data 类型不符应报解析错误");
        assert!(matches!(err, DiscoveryError::Parse(_)), "实际: {err:?}");
    }

    #[tokio::test]
    async fn unsupported_api_reports_unsupported() {
        let err = list_models(Api::Zai, "", "k", client())
            .await
            .expect_err("Zai 应不支持");
        assert!(matches!(err, DiscoveryError::Unsupported { api: Api::Zai }));
    }

    /// RPC 面 JSON 键为 camelCase（`ownedBy`）。
    #[test]
    fn discovered_model_serializes_camel_case() {
        let m = DiscoveredModel {
            id: "gpt-4o".into(),
            owned_by: Some("openai".into()),
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["ownedBy"], "openai");
        let back: DiscoveredModel = serde_json::from_value(v).unwrap();
        assert_eq!(back.id, "gpt-4o");
        assert_eq!(back.owned_by.as_deref(), Some("openai"));
    }
}
