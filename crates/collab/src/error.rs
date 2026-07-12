//! Collab 错误类型。

use thiserror::Error;

/// Collab 操作错误。
#[derive(Debug, Error)]
pub enum CollabError {
    /// 密码学失败（密封/解封）。
    #[error("crypto: {0}")]
    Crypto(String),
    /// 房间密钥非法。
    #[error("invalid room key: {0}")]
    InvalidKey(String),
    /// 帧序列化/反序列化失败。
    #[error("frame codec: {0}")]
    Frame(String),
    /// 房间密钥与密封数据不匹配（认证失败）。
    #[error("seal auth failure (wrong key or tampered)")]
    Auth,
}

impl From<aes_gcm::Error> for CollabError {
    fn from(e: aes_gcm::Error) -> Self {
        Self::Crypto(e.to_string())
    }
}

impl From<serde_json::Error> for CollabError {
    fn from(e: serde_json::Error) -> Self {
        Self::Frame(e.to_string())
    }
}
