//! JSON-RPC `Content-Length` 帧编解码（LSP / DAP 同源格式，H45 收敛）。
//!
//! 帧格式：`Content-Length: <N>\r\n\r\n` + N 字节 JSON body（部分实现用裸 `\n\n`，
//! 本解码器两种行尾都接受）。此前 DAP（`crates/dap/src/frame.rs`）与 LSP
//! （`crates/lsp/src/transport.rs`）各写了一份帧解析/构造——上限、行尾兼容、错误处理
//! 各不相同，故收敛到本模块作为**唯一实现**，两侧只做错误类型映射。
//!
//! [`encode_frame`] 编码；[`decode_frames`] 为**纯增量解码**：给定累计缓冲区切片，返回
//! 「完整帧列表 + 已消费字节数」，调用方 `drain` 消费后继续累积输入，天然支持跨 chunk
//! 分片（一帧拆多次写入、多帧合并在一次读取）。

use serde_json::Value;

/// 单帧 body 长度上限（约 512 MiB，防恶意/损坏对端撑爆内存）。
pub const MAX_BODY_LEN: usize = 512 * 1024 * 1024;
/// 帧头最大长度：超过即视为非本协议字节流（头部没有终止符）。
pub const MAX_HEADER_LEN: usize = 64 * 1024;

/// 帧编解码错误。
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// 头部格式非法或长度超限。
    #[error("帧协议错误: {0}")]
    Protocol(String),
    /// body 非合法 JSON。
    #[error("帧 JSON 解析失败: {0}")]
    Json(#[from] serde_json::Error),
}

/// 将 JSON body 编码为一帧（`Content-Length` 头 + body）。
///
/// # Errors
/// JSON 序列化失败时返回 [`FrameError::Json`]（对 `serde_json::Value` 实际不会发生）。
pub fn encode_frame(body: &Value) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(body)?;
    let mut out = Vec::with_capacity(payload.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 编码为 `String`（调用方直接写文本流时用；body 必为 UTF-8 JSON）。
///
/// # Errors
/// 同 [`encode_frame`]。
pub fn encode_frame_to_string(body: &Value) -> Result<String, FrameError> {
    let bytes = encode_frame(body)?;
    // `encode_frame` 的输出是 ASCII 头 + UTF-8 JSON body，恒为合法 UTF-8。
    Ok(String::from_utf8(bytes).unwrap_or_default())
}

/// 把**已序列化的 JSON 文本**编码为一帧（长度按 UTF-8 字节计）。
///
/// 供调用方在已经持有 JSON 字符串时避免「解析回 Value 再序列化」的往返。
#[must_use]
pub fn encode_str_frame(json: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{json}", json.len())
}

/// 从累计缓冲区解码所有完整帧。
///
/// 返回 `(帧列表, 已消费字节数)`。缓冲区只含不完整帧（头未完整或 body 未收齐）时返回
/// 空列表与 `0`，调用方应继续追加输入后重试。
///
/// # Errors
/// 头格式非法、`Content-Length` 缺失/超限，或 body 非合法 JSON 时返回错误。
pub fn decode_frames(input: &[u8]) -> Result<(Vec<Value>, usize), FrameError> {
    decode_frames_with_limit(input, MAX_BODY_LEN)
}

/// [`decode_frames`] 的可定制 body 上限版本（调用方有更严格预算时用）。
///
/// # Errors
/// 同 [`decode_frames`]。
pub fn decode_frames_with_limit(
    input: &[u8],
    max_body_len: usize,
) -> Result<(Vec<Value>, usize), FrameError> {
    let mut frames = Vec::new();
    let mut consumed = 0usize;
    loop {
        let rest = &input[consumed..];
        if rest.is_empty() {
            break;
        }
        let Some((header_len, body_start)) = find_header_end(rest) else {
            // 头未完整：超过上限视为非本协议字节流，否则等更多数据。
            if rest.len() > MAX_HEADER_LEN {
                return Err(FrameError::Protocol(
                    "未找到帧头终止符（输入疑似非 Content-Length 帧）".into(),
                ));
            }
            break;
        };
        let content_length = parse_content_length(&rest[..header_len])?;
        if content_length > max_body_len {
            return Err(FrameError::Protocol(format!(
                "Content-Length 超限: {content_length} > {max_body_len}"
            )));
        }
        let total = body_start + content_length;
        if rest.len() < total {
            break; // body 未完整，等待更多数据
        }
        let body = &rest[body_start..total];
        let value: Value = serde_json::from_slice(body)?;
        frames.push(value);
        consumed += total;
    }
    Ok((frames, consumed))
}

/// 从 `Content-Length: N` 头区块解析长度（大小写不敏感，容忍多余空白与其它头）。
///
/// # Errors
/// 缺少 `Content-Length` 或数值非法时返回 [`FrameError::Protocol`]。
pub fn parse_content_length(header: &[u8]) -> Result<usize, FrameError> {
    let text = String::from_utf8_lossy(header);
    for line in text.lines() {
        let line = line.trim();
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("content-length") {
            continue;
        }
        return value
            .trim()
            .parse::<usize>()
            .map_err(|_| FrameError::Protocol(format!("Content-Length 非法: {:?}", value.trim())));
    }
    Err(FrameError::Protocol("缺少 Content-Length 头".into()))
}

/// 定位帧头终止符，返回 `(头字节长度, body 起始偏移)`。
///
/// 兼容 `\r\n\r\n` 与 `\n\n` 两种行尾（部分实现对端用裸 `\n`）。
fn find_header_end(buf: &[u8]) -> Option<(usize, usize)> {
    if let Some(pos) = find_subslice(buf, b"\r\n\r\n") {
        return Some((pos, pos + 4));
    }
    find_subslice(buf, b"\n\n").map(|pos| (pos, pos + 2))
}

/// 子切片查找（避免为一次查找引入依赖）。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_and_incremental_decode() {
        let a = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
        let b = json!({"jsonrpc": "2.0", "method": "note"});
        let mut buf = encode_frame(&a).unwrap();
        buf.extend_from_slice(&encode_frame(&b).unwrap());
        let (frames, consumed) = decode_frames(&buf).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["id"], 1);
        assert_eq!(frames[1]["method"], "note");
        assert_eq!(consumed, buf.len());

        // 分片：逐字节喂入，任何前缀都只能产出「已完整」的帧前缀，且消费量不越过缓冲区。
        let first_len = encode_frame(&a).unwrap().len();
        for cut in 1..buf.len() {
            let (frames, consumed) = decode_frames(&buf[..cut]).unwrap();
            assert!(consumed <= cut, "cut={cut} 消费量越界");
            assert!(frames.len() <= 2, "cut={cut} 帧数超界");
            if cut < first_len {
                assert!(
                    frames.is_empty(),
                    "cut={cut} 早于首帧结束时不应产出帧（首帧 {first_len} 字节）"
                );
            }
            if frames.len() == 2 {
                assert_eq!(cut, buf.len(), "两帧齐全只应发生在完整输入处");
            }
        }
    }

    #[test]
    fn accepts_bare_newline_header_terminator() {
        let body = json!({"x": 1});
        let payload = serde_json::to_vec(&body).unwrap();
        let mut raw = format!("Content-Length: {}\n\n", payload.len()).into_bytes();
        raw.extend_from_slice(&payload);
        let (frames, _) = decode_frames(&raw).unwrap();
        assert_eq!(frames[0]["x"], 1);
    }

    #[test]
    fn rejects_bad_header_and_oversized_body() {
        assert!(matches!(
            decode_frames(b"X-Other: 1\r\n\r\n{}"),
            Err(FrameError::Protocol(_))
        ));
        assert!(matches!(
            decode_frames(b"Content-Length: abc\r\n\r\n{}"),
            Err(FrameError::Protocol(_))
        ));
        let huge = format!("Content-Length: {}\r\n\r\n", MAX_BODY_LEN + 1);
        assert!(matches!(
            decode_frames(huge.as_bytes()),
            Err(FrameError::Protocol(_))
        ));
        // 头部无终止符且超过上限 → 非本协议流。
        let junk = vec![b'x'; MAX_HEADER_LEN + 1];
        assert!(matches!(decode_frames(&junk), Err(FrameError::Protocol(_))));
    }

    #[test]
    fn trailing_partial_frame_is_not_consumed() {
        let full = encode_frame(&json!({"id": 1})).unwrap();
        let mut buf = full.clone();
        buf.extend_from_slice(b"Content-Length: 9999\r\n\r\n{\"id\":2}");
        let (frames, consumed) = decode_frames(&buf).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(consumed, full.len());
    }
}
