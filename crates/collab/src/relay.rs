//! 协同中继：按**不透明房间 id** 广播密封字节，从不接触明文/密钥。
//!
//! 移植自 [`oh-my-pi collab-web`](../../../third/oh-my-pi/packages/collab-web) 的 relay 语义
//! （浏览器侧 WebSocket relay；这里提供进程内 [`Relay`] 与 [`CollabClient`]，便于测试与本地直连，
//! WS 接入由上层 `agent-server` 桥接）。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};

use crate::codec::{RoomKey, open, seal};
use crate::error::CollabError;
use crate::frame::WireFrame;
use crate::room::room_id;

/// 默认广播通道容量。
const DEFAULT_CAPACITY: usize = 256;

/// 进程内协同中继：room_id → 广播发送端。
#[derive(Clone)]
pub struct Relay {
    rooms: Arc<Mutex<HashMap<String, broadcast::Sender<Vec<u8>>>>>,
    capacity: usize,
}

impl Relay {
    /// 以默认通道容量构造。
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// 指定通道容量构造。
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            rooms: Arc::new(Mutex::new(HashMap::new())),
            capacity: capacity.max(8),
        }
    }

    /// 加入（或创建）房间，返回订阅接收端。
    ///
    /// 加入时顺便清理同房间无订阅者的旧 sender（防止失效房间累积导致内存泄漏）。
    pub async fn join(&self, room_id: &str) -> broadcast::Receiver<Vec<u8>> {
        let mut rooms = self.rooms.lock().await;
        // 若该房间已存在但已无订阅者，先移除旧 sender 再重建，避免持有陈旧 channel buffer。
        if let Some(existing) = rooms.get(room_id) {
            if existing.receiver_count() == 0 {
                rooms.remove(room_id);
            }
        }
        let sender = rooms
            .entry(room_id.to_string())
            .or_insert_with(|| broadcast::channel(self.capacity).0);
        sender.subscribe()
    }

    /// 向房间广播密封字节；返回送达的活跃订阅数（不含自身已关闭）。
    ///
    /// 若房间无任何订阅者，则视为无效房间并从表中移除（防止泄露）。
    pub async fn publish(&self, room_id: &str, sealed: Vec<u8>) -> usize {
        let sender = {
            let mut rooms = self.rooms.lock().await;
            // 无订阅者的房间不发布，同时清理陈旧 sender。
            if let Some(existing) = rooms.get(room_id) {
                if existing.receiver_count() == 0 {
                    rooms.remove(room_id);
                    return 0;
                }
            }
            rooms
                .entry(room_id.to_string())
                .or_insert_with(|| broadcast::channel(self.capacity).0)
                .clone()
        };
        sender.send(sealed).unwrap_or(0)
    }

    /// 清理所有无订阅者的房间（周期性维护调用，防止长时运行内存增长）。
    ///
    /// 返回被清理的房间数。
    pub async fn cleanup_empty(&self) -> usize {
        let mut rooms = self.rooms.lock().await;
        let before = rooms.len();
        rooms.retain(|_, sender| sender.receiver_count() > 0);
        before - rooms.len()
    }

    /// 当前活跃房间数（含可能未清理的空房间；如需精确值先调用 [`cleanup_empty`](Self::cleanup_empty)）。
    pub async fn room_count(&self) -> usize {
        self.rooms.lock().await.len()
    }
}

impl Default for Relay {
    fn default() -> Self {
        Self::new()
    }
}

/// 协同客户端：密封发送、解封接收，密钥仅本地持有。
pub struct CollabClient {
    relay: Relay,
    key: RoomKey,
    room_id: String,
    client_id: String,
}

impl CollabClient {
    /// 构造：由房间密钥派生不透明 room_id。
    #[must_use]
    pub fn new(relay: Relay, key: RoomKey, client_id: String) -> Self {
        let room_id = room_id(&key);
        Self {
            relay,
            key,
            room_id,
            client_id,
        }
    }

    /// 当前客户端 id。
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// 房间的派生 id（中继路由键）。
    #[must_use]
    pub fn room_id(&self) -> &str {
        &self.room_id
    }

    /// 订阅本房间的密封字节流（供 `recv` 消费）。
    pub async fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.relay.join(&self.room_id).await
    }

    /// 密封并发送一帧；`frame` 的 `client_id`/`ts` 由本客户端覆写。
    ///
    /// # Errors
    /// 密封失败时返回 [`CollabError`]。
    pub async fn send(&self, mut frame: WireFrame) -> Result<usize, CollabError> {
        stamp(&mut frame, &self.client_id);
        let sealed = seal(&self.key, &frame)?;
        Ok(self.relay.publish(&self.room_id, sealed).await)
    }

    /// 解封一条已订阅的密封字节。
    ///
    /// # Errors
    /// 解封失败时返回 [`CollabError`]。
    pub fn decode(&self, sealed: &[u8]) -> Result<WireFrame, CollabError> {
        open(&self.key, sealed)
    }
}

fn stamp(frame: &mut WireFrame, client_id: &str) {
    let ts = now_ms();
    match frame {
        WireFrame::Chat {
            client_id: c,
            ts: t,
            ..
        }
        | WireFrame::Tool {
            client_id: c,
            ts: t,
            ..
        }
        | WireFrame::Presence {
            client_id: c,
            ts: t,
            ..
        }
        | WireFrame::Sync {
            client_id: c,
            ts: t,
            ..
        } => {
            *c = client_id.to_string();
            *t = ts;
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::generate_room_key;

    #[tokio::test]
    async fn two_clients_exchange() {
        let relay = Relay::new();
        let alice = CollabClient::new(relay.clone(), generate_room_key(), "alice".into());
        // 不同密钥 → 不同房间；此处用相同密钥模拟同房间
        let key = generate_room_key();
        let a = CollabClient::new(relay.clone(), key, "alice".into());
        let b = CollabClient::new(relay.clone(), key, "bob".into());
        assert_eq!(a.room_id(), b.room_id());
        assert_ne!(a.room_id(), alice.room_id());

        let mut rx = b.subscribe().await;
        a.send(WireFrame::Chat {
            client_id: String::new(),
            text: "hello".into(),
            ts: 0,
        })
        .await
        .unwrap();

        let sealed = rx.recv().await.unwrap();
        let frame = b.decode(&sealed).unwrap();
        match frame {
            WireFrame::Chat {
                client_id, text, ..
            } => {
                assert_eq!(client_id, "alice");
                assert_eq!(text, "hello");
            }
            _ => panic!("expected Chat"),
        }
    }

    #[tokio::test]
    async fn relay_is_key_blind() {
        // 中继按 room_id 路由：窃听者用错误密钥仍能收到不透明字节，但解封失败。
        let relay = Relay::new();
        let sender_key = generate_room_key();
        let sender = CollabClient::new(relay.clone(), sender_key, "a".into());
        // 窃听者直接订阅发送方派生的 room_id（不经密钥）
        let mut rx = relay.join(sender.room_id()).await;
        sender
            .send(WireFrame::Chat {
                client_id: String::new(),
                text: "secret".into(),
                ts: 0,
            })
            .await
            .unwrap();
        let sealed = rx.recv().await.unwrap();
        assert!(!sealed.is_empty()); // 中继转发了不透明字节
        // 错误密钥无法解封
        let wrong_key = generate_room_key();
        assert!(open(&wrong_key, &sealed).is_err());
        // 正确密钥可以
        assert!(open(&sender_key, &sealed).is_ok());
    }

    #[tokio::test]
    async fn empty_rooms_are_cleaned_up() {
        // 回归：无订阅者的房间必须能被清理，否则长时运行内存无界增长（修复 #1）。
        let relay = Relay::new();
        // 创建一个房间并立即丢弃订阅者
        let room_id = "test-room-1";
        {
            let _rx = relay.join(room_id).await;
            assert_eq!(relay.room_count().await, 1);
        } // _rx drop → 无订阅者
        // cleanup_empty 应清掉空房间
        let removed = relay.cleanup_empty().await;
        assert_eq!(removed, 1);
        assert_eq!(relay.room_count().await, 0);
    }

    #[tokio::test]
    async fn publish_to_empty_room_returns_zero_and_cleans() {
        // 回归：向无订阅者的房间 publish 应返回 0 并清理 sender，不泄露（修复 #1）。
        let relay = Relay::new();
        let room_id = "test-room-2";
        // 先 join 一次创建房间，再丢弃订阅者
        drop(relay.join(room_id).await);
        assert_eq!(relay.room_count().await, 1);
        // publish 到空房间
        let delivered = relay.publish(room_id, vec![1, 2, 3]).await;
        assert_eq!(delivered, 0, "无订阅者时不应送达");
        assert_eq!(relay.room_count().await, 0, "空房间 sender 应被清理");
    }

    #[tokio::test]
    async fn active_room_survives_cleanup() {
        // 回归：仍有活跃订阅者的房间不应被误清。
        let relay = Relay::new();
        let room_id = "test-room-3";
        let _rx = relay.join(room_id).await; // 持有订阅者
        let removed = relay.cleanup_empty().await;
        assert_eq!(removed, 0);
        assert_eq!(relay.room_count().await, 1);
    }
}
