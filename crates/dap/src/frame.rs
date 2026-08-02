//! DAP `Content-Length` 帧编解码。
//!
//! DAP 与 LSP 同源的帧格式：`Content-Length: <N>\r\n\r\n` + N 字节 JSON body。
//! [`encode_frame`] 负责编码；[`decode_frames`] 为纯增量解码——给定累计缓冲区切片，
//! 返回「完整帧列表 + 已消费字节数」，调用方 `drain` 消费后继续累积输入，天然支持
//! 跨 chunk 分片（一帧拆多次写入、多帧合并在一次读取）。

use serde_json::Value;

use crate::DapError;

/// 单帧 body 长度上限（约 512 MiB，防恶意/损坏适配器撑爆内存）。
const MAX_BODY_LEN: usize = 512 * 1024 * 1024;
/// 帧头最大长度：64 KiB 内必须出现头终止符，否则视为非 DAP 字节流。
const MAX_HEADER_LEN: usize = 64 * 1024;

/// 将 JSON body 编码为一帧（`Content-Length` 头 + body）。
///
/// # Errors
/// JSON 序列化失败（对 `serde_json::Value` 实际不会发生）。
pub fn encode_frame(body: &Value) -> Result<Vec<u8>, DapError> {
    let payload = serde_json::to_vec(body)?;
    let mut out = Vec::with_capacity(payload.len() + 64);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 从累计缓冲区解码所有完整帧。
///
/// 返回 `(帧列表, 已消费字节数)`。缓冲区中只含不完整帧（头未完整或 body 未收齐）时
/// 返回空列表与 `0`，调用方应继续追加输入后重试。
///
/// # Errors
/// 头格式非法、`Content-Length` 缺失/超限，或 body 非合法 JSON 时返回
/// [`DapError::Protocol`] / [`DapError::Json`]。
pub fn decode_frames(input: &[u8]) -> Result<(Vec<Value>, usize), DapError> {
    let mut frames = Vec::new();
    let mut consumed = 0usize;
    loop {
        let rest = &input[consumed..];
        if rest.is_empty() {
            break;
        }
        let Some((header_len, body_start)) = find_header_end(rest) else {
            // 头未完整：超过上限视为非 DAP 字节流，否则等更多数据
            if rest.len() > MAX_HEADER_LEN {
                return Err(DapError::Protocol(
                    "未找到帧头终止符（输入疑似非 DAP 字节流）".into(),
                ));
            }
            break;
        };
        let content_length = parse_content_length(&rest[..header_len])?;
        if content_length > MAX_BODY_LEN {
            return Err(DapError::Protocol(format!("Content-Length 超限: {content_length}")));
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

/// 定位帧头终止符，返回 `(头字节长度, body 起始偏移)`。
///
/// 兼容 `\r\n\r\n` 与 `\n\n` 两种行尾（部分适配器用裸 `\n`）。
fn find_header_end(buf: &[u8]) -> Option<(usize, usize)> {
    if let Some(pos) = find_subslice(buf, b"\r\n\r\n") {
        return Some((pos, pos + 4));
    }
    find_subslice(buf, b"\n\n").map(|pos| (pos, pos + 2))
}

/// 朴素子串定位。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 从头字节中解析 `Content-Length`（头名大小写不敏感，行尾 `\r\n` 或 `\n` 均可）。
fn parse_content_length(header: &[u8]) -> Result<usize, DapError> {
    let text = String::from_utf8_lossy(header);
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| DapError::Protocol(format!("Content-Length 头非法: {line:?}")))?;
            return Ok(n);
        }
    }
    Err(DapError::Protocol("缺少 Content-Length 头".into()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn frame(msg: &Value) -> Vec<u8> {
        encode_frame(msg).expect("编码不应失败")
    }

    #[test]
    fn encode_decode_round_trip() {
        let msg = json!({ "seq": 1, "type": "request", "command": "initialize" });
        let bytes = frame(&msg);
        assert!(bytes.starts_with(b"Content-Length: "));
        let (frames, consumed) = decode_frames(&bytes).expect("解码不应失败");
        assert_eq!(frames, vec![msg]);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn decode_byte_by_byte_cross_chunk() {
        let a = json!({ "seq": 1, "type": "event", "event": "stopped" });
        let b = json!({ "seq": 2, "type": "event", "event": "continued" });
        let stream = [frame(&a), frame(&b)].concat();
        let mut buf = Vec::new();
        let mut got = Vec::new();
        for byte in stream {
            buf.push(byte);
            let (frames, consumed) = decode_frames(&buf).expect("解码不应失败");
            buf.drain(..consumed);
            got.extend(frames);
        }
        assert_eq!(got, vec![a, b]);
        assert!(buf.is_empty());
    }

    #[test]
    fn decode_multiple_frames_in_one_buffer() {
        let a = json!({ "seq": 1, "type": "event", "event": "output" });
        let b = json!({ "seq": 2, "type": "event", "event": "thread" });
        let stream = [frame(&a), frame(&b)].concat();
        let (frames, consumed) = decode_frames(&stream).expect("解码不应失败");
        assert_eq!(frames, vec![a, b]);
        assert_eq!(consumed, stream.len());
    }

    #[test]
    fn header_variants_accepted() {
        let body = br#"{"seq":1,"type":"event","event":"stopped"}"#;
        // 裸 \n 行尾 + 小写头名
        let mut raw = format!("content-length: {}\n\n", body.len()).into_bytes();
        raw.extend_from_slice(body);
        let (frames, consumed) = decode_frames(&raw).expect("解码不应失败");
        assert_eq!(frames.len(), 1);
        assert_eq!(consumed, raw.len());
        // \r\n 行尾 + 附带 Content-Type 头
        let mut raw2 =
            format!("Content-Length: {}\r\nContent-Type: application/json\r\n\r\n", body.len()).into_bytes();
        raw2.extend_from_slice(body);
        let (frames2, consumed2) = decode_frames(&raw2).expect("解码不应失败");
        assert_eq!(frames2.len(), 1);
        assert_eq!(consumed2, raw2.len());
    }

    #[test]
    fn incomplete_frames_wait_for_more() {
        let msg = json!({ "seq": 1 });
        let bytes = frame(&msg);
        // 头未完整
        let (frames, consumed) = decode_frames(&bytes[..10]).expect("不完整头应返回空");
        assert!(frames.is_empty());
        assert_eq!(consumed, 0);
        // body 缺尾字节
        let (frames2, consumed2) =
            decode_frames(&bytes[..bytes.len() - 1]).expect("不完整 body 应返回空");
        assert!(frames2.is_empty());
        assert_eq!(consumed2, 0);
    }

    #[test]
    fn missing_content_length_is_protocol_error() {
        let raw = b"Content-Type: application/json\r\n\r\n{}";
        assert!(matches!(decode_frames(raw), Err(DapError::Protocol(_))));
    }

    #[test]
    fn oversized_content_length_is_protocol_error() {
        let raw = format!("Content-Length: {}\r\n\r\n", MAX_BODY_LEN + 1);
        assert!(matches!(decode_frames(raw.as_bytes()), Err(DapError::Protocol(_))));
    }

    #[test]
    fn garbage_stream_is_protocol_error() {
        let garbage = vec![b'x'; MAX_HEADER_LEN + 1];
        assert!(matches!(decode_frames(&garbage), Err(DapError::Protocol(_))));
    }
}
