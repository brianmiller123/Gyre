//! AES-256-GCM 密封编解码器：房间密钥仅存于链接片段，中继只见不透明字节。
//!
//! 移植自 [`oh-my-pi collab-web/codec.ts`](../../../third/oh-my-pi/packages/collab-web/src/lib/codec.ts)
//! （浏览器侧 WebCrypto 的 Rust 对偶）。
//!
//! 密封布局：`[12B nonce][ciphertext+tag]`。房间密钥为 32 字节。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;

use crate::error::CollabError;
use crate::frame::WireFrame;

/// Nonce 长度（字节）。
///
/// AES-GCM 标准 nonce 为 96 位；此处采用纯随机 nonce，无计数器/去重，故同一
/// 房间密钥的加密次数存在安全上限（见 [`seal`] 的 # 安全 段落）。
pub const NONCE_LEN: usize = 12;
/// 房间密钥长度（字节）。
pub const KEY_LEN: usize = 32;

/// 房间密钥（32 字节）。
///
/// 房间密钥在房间生命周期内保持不变，作为该房间所有帧的 AES-256-GCM 单密钥。
/// 由于 [`seal`] 使用随机 nonce 且无去重/计数，同一密钥的加密次数不宜超过
/// 2³² 次；长期复用同一房间时应在达到上限前生成新房间（轮换密钥）。
pub type RoomKey = [u8; KEY_LEN];

/// 随机生成一个房间密钥。
///
/// 用于新房间初始化。当旧房间接近随机 nonce 安全上限（2³² 次加密）时，
/// 应创建新房间（生成新密钥）而非继续复用旧密钥。
#[must_use]
pub fn generate_room_key() -> RoomKey {
    let mut key = [0u8; KEY_LEN];
    rand::thread_rng().fill_bytes(&mut key);
    key
}

/// 用房间密钥密封一帧 → 不透明字节。
///
/// # 安全
/// 使用 12 字节（96 位）随机 nonce。按 AES-GCM 安全性要求，**同一密钥**下
/// 随机 nonce 的加密次数不宜超过 2³² 次（碰撞概率生日界 ≈ 2⁴⁸ 次 nonce 时
/// 显著上升）。nonce 重用 + 相同密钥将**灾难性破坏**机密性与完整性。当前
/// 实现无消息计数或去重检测，房间密钥在房间生命周期内不变——长期复用同一
/// 房间时应在达到上限前轮换密钥（生成新房间）。
///
/// # Errors
/// 序列化或加密失败时返回 [`CollabError`]。
pub fn seal(key: &RoomKey, frame: &WireFrame) -> Result<Vec<u8>, CollabError> {
    let plaintext = serde_json::to_vec(frame)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext.as_ref())?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// 用房间密钥解封字节 → 帧。
///
/// # Errors
/// 长度不足、认证失败（密钥不符/被篡改）或反序列化失败时返回 [`CollabError`]。
pub fn open(key: &RoomKey, data: &[u8]) -> Result<WireFrame, CollabError> {
    if data.len() <= NONCE_LEN {
        return Err(CollabError::Crypto("sealed frame too short".into()));
    }
    let (nonce_bytes, ciphertext) = data.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|_| CollabError::Auth)?;
    let frame = serde_json::from_slice(&plaintext)?;
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WireFrame {
        WireFrame::Chat {
            client_id: "alice".into(),
            text: "hi".into(),
            ts: 1,
        }
    }

    #[test]
    fn seal_open_roundtrip() {
        let key = generate_room_key();
        let sealed = seal(&key, &sample()).unwrap();
        assert!(sealed.len() > NONCE_LEN);
        let opened = open(&key, &sealed).unwrap();
        assert_eq!(opened, sample());
    }

    #[test]
    fn wrong_key_auth_fails() {
        let key = generate_room_key();
        let other = generate_room_key();
        let sealed = seal(&key, &sample()).unwrap();
        assert!(matches!(open(&other, &sealed), Err(CollabError::Auth)));
    }

    #[test]
    fn tampered_ciphertext_auth_fails() {
        let key = generate_room_key();
        let mut sealed = seal(&key, &sample()).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xFF;
        assert!(matches!(open(&key, &sealed), Err(CollabError::Auth)));
    }
}
