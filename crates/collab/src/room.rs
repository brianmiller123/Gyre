//! 房间密钥的 URL 片段编码与不透明房间 id 派生。
//!
//! 房间密钥经 base64url（无填充）编码后放入分享链接的 `#` 片段——
//! 浏览器/客户端不会把片段发给服务器，故中继永远拿不到密钥。
//!
//! 中继按**不透明房间 id** 路由：`room_id = SHA-256(key)` 的十六进制前缀，
//! 它足以区分房间、却不可逆推密钥。

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::codec::RoomKey;
use crate::error::CollabError;

/// 房间 id 字节长度（SHA-256 截断前 16 字节 = 32 hex）。
const ROOM_ID_BYTES: usize = 16;

/// 把房间密钥编码为 base64url（无填充），用于放入 URL 片段。
#[must_use]
pub fn encode_room_key(key: &RoomKey) -> String {
    URL_SAFE_NO_PAD.encode(key)
}

/// 从 base64url 文本解码房间密钥。
///
/// # Errors
/// 长度/编码非法时返回 [`CollabError::InvalidKey`]。
pub fn decode_room_key(s: &str) -> Result<RoomKey, CollabError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| CollabError::InvalidKey(e.to_string()))?;
    if bytes.len() != crate::codec::KEY_LEN {
        return Err(CollabError::InvalidKey(format!(
            "key must be {} bytes, got {}",
            crate::codec::KEY_LEN,
            bytes.len()
        )));
    }
    let mut key = [0u8; crate::codec::KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// 派生不透明房间 id（SHA-256 前 16 字节的 32 位十六进制）。
#[must_use]
pub fn room_id(key: &RoomKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key);
    let digest = hasher.finalize();
    hex_lower(&digest[..ROOM_ID_BYTES])
}

/// 构造分享链接：`<base>#<encoded-key>`。
#[must_use]
pub fn build_share_url(base: &str, key: &RoomKey) -> String {
    format!("{base}#{}", encode_room_key(key))
}

/// 从分享链接解析房间密钥（取 `#` 后片段）。
///
/// # Errors
/// 无片段或解码失败时返回 [`CollabError`]。
pub fn parse_share_url(url: &str) -> Result<RoomKey, CollabError> {
    let fragment = url
        .split_once('#')
        .map(|(_, f)| f)
        .ok_or_else(|| CollabError::InvalidKey("share url missing # fragment".into()))?;
    decode_room_key(fragment)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::generate_room_key;

    #[test]
    fn key_roundtrip_through_fragment() {
        let key = generate_room_key();
        let enc = encode_room_key(&key);
        let dec = decode_room_key(&enc).unwrap();
        assert_eq!(dec, key);
    }

    #[test]
    fn share_url_roundtrip() {
        let key = generate_room_key();
        let url = build_share_url("https://collab.app/r/abc", &key);
        assert!(url.contains('#'));
        assert_eq!(parse_share_url(&url).unwrap(), key);
    }

    #[test]
    fn room_id_stable_and_opaque() {
        let key = generate_room_key();
        let id = room_id(&key);
        assert_eq!(id.len(), ROOM_ID_BYTES * 2);
        assert_eq!(id, room_id(&key));
        assert_ne!(id, room_id(&generate_room_key()));
    }
}
