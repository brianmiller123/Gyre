//! DAP `Content-Length` 帧编解码（薄包装）。
//!
//! 帧格式与实现**收敛到 [`agent_core::jsonrpc_frame`]**（H45：此前 DAP 与 LSP 各写
//! 一份帧解析/构造，上限与行尾兼容各不相同）。本模块只保留 DAP 的错误类型映射，
//! 公共 API（[`encode_frame`] / [`decode_frames`]）不变。

use serde_json::Value;

use crate::DapError;

/// 将 JSON body 编码为一帧（`Content-Length` 头 + body）。
///
/// # Errors
/// JSON 序列化失败时返回 [`DapError::Json`]。
pub fn encode_frame(body: &Value) -> Result<Vec<u8>, DapError> {
    agent_core::jsonrpc_frame::encode_frame(body).map_err(frame_err)
}

/// 从累计缓冲区解码所有完整帧（详见 [`agent_core::jsonrpc_frame::decode_frames`]）。
///
/// # Errors
/// 头格式非法、`Content-Length` 缺失/超限，或 body 非合法 JSON 时返回
/// [`DapError::Protocol`] / [`DapError::Json`]。
pub fn decode_frames(input: &[u8]) -> Result<(Vec<Value>, usize), DapError> {
    agent_core::jsonrpc_frame::decode_frames(input).map_err(frame_err)
}

/// 共享帧错误 → DAP 错误。
fn frame_err(e: agent_core::jsonrpc_frame::FrameError) -> DapError {
    match e {
        agent_core::jsonrpc_frame::FrameError::Json(e) => DapError::Json(e),
        agent_core::jsonrpc_frame::FrameError::Protocol(msg) => DapError::Protocol(msg),
    }
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
        let mut raw2 = format!(
            "Content-Length: {}\r\nContent-Type: application/json\r\n\r\n",
            body.len()
        )
        .into_bytes();
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
        let raw = format!(
            "Content-Length: {}\r\n\r\n",
            agent_core::jsonrpc_frame::MAX_BODY_LEN + 1
        );
        assert!(matches!(
            decode_frames(raw.as_bytes()),
            Err(DapError::Protocol(_))
        ));
    }

    #[test]
    fn garbage_stream_is_protocol_error() {
        let garbage = vec![b'x'; agent_core::jsonrpc_frame::MAX_HEADER_LEN + 1];
        assert!(matches!(
            decode_frames(&garbage),
            Err(DapError::Protocol(_))
        ));
    }
}
