//! 图片工具：read_image（读取本地图片为多模态结果）+ image_gen（调用 OpenAI 兼容图像生成）。
//!
//! 多模态链路：工具返回 [`ToolResult::Image`]，由 `convert_to_llm` 编码为 base64
//! [`ToolImage`](agent_core::ToolImage)，支持多模态的 provider（Anthropic）可作为 image block
//! 真实传递；OpenAI tool role 仅支持文本，自动降级为占位提示。

use std::path::Path;

use agent_core::{CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{Tool, ToolContext};

/// 单张图片大小上限（10 MiB）。
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// 按扩展名推断图片 MIME。
fn mime_from_ext(path: &Path) -> Option<String> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png".into()),
        Some("jpg") | Some("jpeg") => Some("image/jpeg".into()),
        Some("gif") => Some("image/gif".into()),
        Some("webp") => Some("image/webp".into()),
        _ => None,
    }
}

/// 读取本地图片，返回多模态图像结果（支持多模态的 provider 可"看到"图像）。
pub struct ReadImageTool;

#[async_trait]
impl Tool for ReadImageTool {
    fn name(&self) -> &str {
        "read_image"
    }
    fn description(&self) -> &str {
        "读取本地图片文件（png/jpeg/gif/webp），作为图像供模型查看分析。仅限只读。"
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "图片路径（相对工作区根或绝对路径）" }
            },
            "required": ["path"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        CapabilityTier::ReadOnly
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `path` 参数".into()))?;
        let full = ctx.workspace.resolve(Path::new(path));
        let mime = mime_from_ext(&full).ok_or_else(|| {
            ToolError::Execution("不支持的图片格式（仅 png/jpeg/gif/webp）".into())
        })?;
        let bytes = tokio::fs::read(&full).await.map_err(ToolError::Io)?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ToolError::Execution(format!(
                "图片过大（{} 字节，上限 {}）",
                bytes.len(),
                MAX_IMAGE_BYTES
            )));
        }
        Ok(ToolResult::Image { mime, data: bytes })
    }
}

/// 调用 OpenAI 兼容图像生成 API（`/images/generations`）。
///
/// 凭据从环境变量读取（`IMAGE_API_KEY` / `OPENAI_API_KEY`）；`base_url` 可由参数覆盖，
/// 兼容网关/本地模型。生成图片以 base64 取回后解码为图像结果。
pub struct ImageGenTool;

#[async_trait]
impl Tool for ImageGenTool {
    fn name(&self) -> &str {
        "image_gen"
    }
    fn description(&self) -> &str {
        "调用图像生成 API（OpenAI 兼容 /images/generations）按 prompt 生成图片。\
         需配置环境变量 IMAGE_API_KEY 或 OPENAI_API_KEY。"
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt":   { "type": "string", "description": "图像描述（prompt）" },
                "size":     { "type": "string", "description": "尺寸如 1024x1024（可选，默认 1024x1024）" },
                "model":    { "type": "string", "description": "模型如 dall-e-3（可选）" }
            },
            "required": ["prompt"]
        })
    }
    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Network
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `prompt` 参数".into()))?;
        let size = input
            .get("size")
            .and_then(Value::as_str)
            .unwrap_or("1024x1024");
        let model = input
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("dall-e-3");
        // base_url 仅从环境变量读取（IMAGE_API_BASE_URL / OPENAI_API_BASE_URL），
        // 绝不接受工具输入参数中的 base_url，防止 prompt 注入将 API Key 发往攻击者地址（凭据泄露 + SSRF）。
        let base_url = std::env::var("IMAGE_API_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_API_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

        let api_key = std::env::var("IMAGE_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .map_err(|_| {
                ToolError::Execution("未设置环境变量 IMAGE_API_KEY / OPENAI_API_KEY".into())
            })?;

        let url = format!("{}/images/generations", base_url.trim_end_matches('/'));
        let body = json!({ "model": model, "prompt": prompt, "n": 1, "size": size, "response_format": "b64_json" });
        let client = shared_image_client();
        let resp = client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Execution(format!("请求失败: {e}")))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(ToolError::Execution(format!("HTTP {st}: {txt}")));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Execution(format!("解析响应失败: {e}")))?;
        let b64 = v["data"][0]["b64_json"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("响应缺少 data[0].b64_json".into()))?;
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| ToolError::Execution(format!("base64 解码失败: {e}")))?;
        // 防止恶意/被劫持上游返回超大图像数据导致 OOM（与 read_image 工具一致的上限）。
        if data.len() > MAX_IMAGE_BYTES {
            return Err(ToolError::Execution(format!(
                "生成的图片过大（{} 字节，上限 {}）",
                data.len(),
                MAX_IMAGE_BYTES
            )));
        }
        Ok(ToolResult::Image {
            mime: "image/png".into(),
            data,
        })
    }
}

/// 图像生成的共享 HTTP 客户端（带连接/整体超时与连接池复用，避免每次请求新建 client
/// 且杜绝上游挂起导致永久阻塞）。
fn shared_image_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("构建图像 HTTP 客户端失败")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ApprovalDecision, ApprovalRequest, Workspace};

    fn dummy_ctx<'a>(ws: &'a Workspace) -> ToolContext<'a> {
        struct AutoApprove;
        #[async_trait::async_trait]
        impl agent_core::ApprovalPolicy for AutoApprove {
            fn decide(&self, _r: &ApprovalRequest<'_>) -> ApprovalDecision {
                ApprovalDecision::Allow
            }
            async fn prompt(
                &self,
                _a: &agent_core::AskMessage,
            ) -> Result<agent_core::AskResponse, ToolError> {
                Ok(agent_core::AskResponse::Yes)
            }
        }
        static CANCEL: std::sync::OnceLock<tokio_util::sync::CancellationToken> =
            std::sync::OnceLock::new();
        let cancel = CANCEL.get_or_init(tokio_util::sync::CancellationToken::new);
        ToolContext {
            workspace: ws,
            approval: &AutoApprove,
            cancel,
            skills: None,
            memory: None,
            resources: None,
            write_effect: None,
        }
    }

    #[tokio::test]
    async fn read_image_returns_image_result() {
        let dir = std::env::temp_dir().join(format!("agent-img-{}", uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        // 写入最小合法 PNG（1x1 透明）的字节
        let png: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        std::fs::write(dir.join("x.png"), png).unwrap();
        let ws = Workspace::new(&dir);
        let ctx = dummy_ctx(&ws);
        let res = ReadImageTool
            .execute(json!({"path":"x.png"}), &ctx)
            .await
            .unwrap();
        match res {
            ToolResult::Image { mime, data } => {
                assert_eq!(mime, "image/png");
                assert_eq!(data, png);
            }
            _ => panic!("应为 Image 结果"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_image_rejects_unsupported_ext() {
        let dir = std::env::temp_dir().join(format!("agent-img2-{}", uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.txt"), "nope").unwrap();
        let ws = Workspace::new(&dir);
        let ctx = dummy_ctx(&ws);
        let res = ReadImageTool.execute(json!({"path":"x.txt"}), &ctx).await;
        assert!(res.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn uuid() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{nanos:x}")
    }
}
