//! # agent-collab
//!
//! Collab Web：端到端加密的协同会话中继。
//!
//! 移植自 [`oh-my-pi collab-web`](../../../third/oh-my-pi/packages/collab-web)（浏览器侧 WebCrypto seal/open 的 Rust 对偶）。
//!
//! 模型：房间密钥仅存于分享链接的 `#` 片段；客户端用它密封 [`WireFrame`] 后发送不透明字节给
//! [`Relay`]；中继按**不透明房间 id**（`SHA-256(key)`）广播，从不接触明文/密钥。
//!
//! - [`codec`] —— AES-256-GCM seal/open + 房间密钥
//! - [`room`] —— URL 片段编码 + 不透明房间 id 派生
//! - [`relay`] —— 进程内中继 + [`CollabClient`]
//! - [`frame`] —— 线协议帧

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

pub mod codec;
pub mod error;
pub mod frame;
pub mod relay;
pub mod room;

pub use codec::{KEY_LEN, NONCE_LEN, RoomKey, generate_room_key, open, seal};
pub use error::CollabError;
pub use frame::WireFrame;
pub use relay::{CollabClient, Relay};
pub use room::{build_share_url, decode_room_key, encode_room_key, parse_share_url, room_id};
