//! # agent-llm
//!
//! LLM Provider 适配器层：[`ProviderRegistry`] 单一分发入口（移植 oh-my-pi `streamSimple`）
//! + 各线协议适配器。当前实现 OpenAI Chat Completions（覆盖最广，含兼容网关/本地 vLLM）。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

mod anthropic;
mod deepseek;
mod glm;
mod openai;
mod plugin;
mod registry;
pub mod transform;

pub use anthropic::AnthropicMessagesAdapter;
pub use deepseek::DeepSeekProvider;
pub use glm::GlmProvider;
pub use openai::OpenAiCompletionsAdapter;
pub use plugin::{collect_providers, LlmProviderPlugin};
pub use registry::ProviderRegistry;
pub use transform::{
    anthropic_apply_cache, anthropic_system_blocks, count_cache_breakpoints, inject_ephemeral_cache,
    normalize_tool_schema, CacheStrategy,
};

/// 读取错误响应体为字符串，限制在 4 KiB 以内（防止异常上游用超大错误体撑爆内存）。
pub(crate) async fn read_error_body(resp: reqwest::Response) -> String {
    const MAX_ERR_BYTES: usize = 4 * 1024;
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                buf.extend_from_slice(&chunk);
                if buf.len() >= MAX_ERR_BYTES {
                    buf.truncate(MAX_ERR_BYTES);
                    break;
                }
            }
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// 从原始字节缓冲中切出下一行（连同尾随 `\n` 一并移除），返回该行的字节。
///
/// 专为流式 SSE 解析设计：以字节 `\n` 为边界切分，保证每次取出的「行」都是完整的
/// UTF-8 序列——从而杜绝 TCP/TLS chunk 把一个多字节字符（中文/emoji）切到两个 chunk
/// 时，`str::from_utf8` 整段丢弃导致增量文本与 JSON 帧损坏的问题。
/// 调用方对取出的字节用 [`String::from_utf8_lossy`] 解码即可安全得到文本行。
pub(crate) fn drain_line(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let nl = buf.iter().position(|&b| b == b'\n')?;
    Some(buf.drain(..=nl).collect())
}

/// SSE 单行最大字节数：超过即视为上游异常（如无换行的超长行），防止缓冲无界增长导致 OOM。
pub(crate) const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;

/// 判断行缓冲是否超过单行上限。供各 Provider 在 `drain_line` 抽干完整行后，对剩余的
/// 未完结尾段做检查——若尾段超限说明上游发了一条无换行的巨型行。
pub(crate) fn line_buffer_too_long(buf: &[u8]) -> bool {
    buf.len() > MAX_SSE_LINE_BYTES
}

/// 将 `extra_body`（per-model 配置的额外请求体字段）合并到请求体顶层。
///
/// 仅当 `extra` 为 JSON 对象时执行合并；其每个键值对直接插入 `body` 顶层，
/// 覆盖同名的既有字段（用于传递 Provider 特有的非标准参数，如 vLLM 的
/// `chat_template_kwargs`）。
pub(crate) fn merge_extra_body(body: &mut serde_json::Value, extra: Option<&serde_json::Value>) {
    if let Some(extra) = extra {
        if let (Some(body_obj), Some(extra_obj)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in extra_obj {
                body_obj.insert(k.clone(), v.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::drain_line;

    #[test]
    fn drain_line_splits_on_newline_and_consumes() {
        let mut buf: Vec<u8> = b"data: hello\nrest".to_vec();
        let line = drain_line(&mut buf).unwrap();
        assert_eq!(String::from_utf8(line).unwrap().trim(), "data: hello");
        assert_eq!(buf, b"rest", "已切出的行应从缓冲移除");
    }

    #[test]
    fn drain_line_returns_none_without_newline() {
        let mut buf: Vec<u8> = b"no newline here".to_vec();
        assert!(drain_line(&mut buf).is_none());
        assert_eq!(buf, b"no newline here", "无换行时不应消费缓冲");
    }

    #[test]
    fn cross_chunk_utf8_survives_byte_buffering() {
        // 回归 H3：跨 chunk 的多字节 UTF-8（中文「你好」）被切到两个 chunk 时，
        // 字节缓冲 + 按 \n 切行必须完整重建字符，而非像旧版 str::from_utf8 整段丢弃。
        // 「你」= E4 BD A0，「好」= E5 A5 BD；在「你」的第 2 字节处切开。
        let full = "data: 你好\n".as_bytes().to_vec();
        let split = 6 + 2; // "data: "（6 字节）+ E4 BD（「你」前 2 字节）
        let prefix = &full[..split];
        let suffix = &full[split..];

        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(prefix);
        // 第一个 chunk 末尾是残缺的多字节字符、且无换行 → 切不出行（不丢字节）。
        assert!(drain_line(&mut buf).is_none());
        buf.extend_from_slice(suffix);
        let line = drain_line(&mut buf).unwrap();
        let text = String::from_utf8(line).unwrap();
        assert!(
            text.contains("你好"),
            "跨 chunk 的中文被损坏: {text:?}"
        );
    }

    #[test]
    fn line_buffer_guard_detects_oversize() {
        // 回归：超过单行上限的缓冲必须被检测到，防 SSE 无换行超长行 OOM。
        let big = vec![b'a'; super::MAX_SSE_LINE_BYTES + 1];
        assert!(super::line_buffer_too_long(&big));
        assert!(!super::line_buffer_too_long(b"small"));
    }
}
