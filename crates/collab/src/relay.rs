//! 协同中继：按**不透明房间 id** 广播密封字节，从不接触明文/密钥。
//!
//! 移植自 [`oh-my-pi collab-web`](../../../third/oh-my-pi/packages/collab-web) 的 relay 语义
//! （浏览器侧 WebSocket relay；这里提供进程内 [`Relay`] 与 [`CollabClient`]，便于测试与本地直连，
//! WS 接入由上层 `agent-server` 桥接）。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};

use crate::codec::{RoomKey, open, seal};
use crate::error::CollabError;
use crate::frame::WireFrame;
use crate::room::room_id;

/// 默认广播通道容量。
const DEFAULT_CAPACITY: usize = 256;

/// 每房间历史重放上限（密封帧条数）。
///
/// 新订阅者（含浏览器 guest 重连）先收到最近 `REPLAY_LIMIT` 条历史再收实时帧；
/// 快照分块单帧可达 48 KiB，200 条最坏约 9.6 MiB/房间，由 60s 清理周期回收。
const DEFAULT_REPLAY_LIMIT: usize = 200;

/// write token 长度（字节）。
const WRITE_TOKEN_LEN: usize = 16;

/// 生成新的写权限令牌（16 字节 CSPRNG → 32 字符十六进制）。
///
/// 令牌随**可写**分享链接传播；只读 view 链接只含房间密钥片段，不含令牌。
#[must_use]
pub fn generate_write_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; WRITE_TOKEN_LEN];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .fold(String::with_capacity(WRITE_TOKEN_LEN * 2), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// 房间状态：广播发送端 + 密封帧历史环（供新订阅者重放）。
struct Room {
    sender: broadcast::Sender<Vec<u8>>,
    history: VecDeque<Vec<u8>>,
}

/// 进程内协同中继：`room_id` → 广播发送端 + 可选 write token 注册表。
#[derive(Clone)]
pub struct Relay {
    rooms: Arc<Mutex<HashMap<String, Room>>>,
    /// `room_id` → 写权限令牌。**有记录的房间 = 受管房间**：无令牌的发布被拒。
    /// 未注册的房间保持开放（向后兼容 demo 语义）。
    tokens: Arc<Mutex<HashMap<String, String>>>,
    capacity: usize,
    replay_limit: usize,
}

impl Relay {
    /// 以默认通道容量与历史重放上限构造。
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// 指定通道容量构造。
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_replay(capacity, DEFAULT_REPLAY_LIMIT)
    }

    /// 指定通道容量与历史重放上限构造。
    #[must_use]
    pub fn with_capacity_and_replay(capacity: usize, replay_limit: usize) -> Self {
        Self {
            rooms: Arc::new(Mutex::new(HashMap::new())),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            capacity: capacity.max(8),
            replay_limit,
        }
    }

    /// 加入（或创建）房间，返回订阅接收端（不带历史重放）。
    ///
    /// 等价于 [`join_with_replay`](Self::join_with_replay) 丢弃历史部分。
    pub async fn join(&self, room_id: &str) -> broadcast::Receiver<Vec<u8>> {
        self.join_with_replay(room_id).await.1
    }

    /// 加入（或创建）房间，返回**历史密封帧** + 实时订阅接收端。
    ///
    /// 历史与订阅在同一锁内原子完成：期间发布的帧不会漏进历史与实时间的缝隙。
    /// 房间仍存活（哪怕当前无订阅者）时复用既有 sender 与历史——掉线期间的帧
    /// 由 [`publish_with_token`](Self::publish_with_token) 持续记入历史，重连可完整找回；
    /// 无订阅者的空房间由 [`cleanup_empty`](Self::cleanup_empty) 统一回收。
    pub async fn join_with_replay(
        &self,
        room_id: &str,
    ) -> (Vec<Vec<u8>>, broadcast::Receiver<Vec<u8>>) {
        let mut rooms = self.rooms.lock().await;
        let room = rooms.entry(room_id.to_string()).or_insert_with(|| Room {
            sender: broadcast::channel(self.capacity).0,
            history: VecDeque::new(),
        });
        let history = room.history.iter().cloned().collect();
        let rx = room.sender.subscribe();
        drop(rooms); // 历史与订阅在同一锁内完成后尽早释放
        (history, rx)
    }

    /// 向房间广播密封字节；返回送达的活跃订阅数（不含自身已关闭）。
    ///
    /// 若房间无任何订阅者，则视为无效房间并从表中移除（防止泄露）。
    /// 等价于 [`publish_with_token`](Self::publish_with_token) 传 `None`：
    /// 受管房间（已 [`set_write_token`](Self::set_write_token)）拒绝无令牌发布。
    ///
    /// # Errors
    /// 受管房间且未提供令牌时返回 [`CollabError::WriteForbidden`]。
    pub async fn publish(&self, room_id: &str, sealed: Vec<u8>) -> Result<usize, CollabError> {
        self.publish_with_token(room_id, sealed, None).await
    }

    /// 向房间广播密封字节（带写权限校验）。
    ///
    /// 受管房间要求 `token` 与注册令牌一致，否则返回 [`CollabError::WriteForbidden`]；
    /// 未注册房间放行（向后兼容）。返回送达的活跃订阅数。
    ///
    /// # Errors
    /// 受管房间令牌不匹配时返回 [`CollabError::WriteForbidden`]。
    pub async fn publish_with_token(
        &self,
        room_id: &str,
        sealed: Vec<u8>,
        token: Option<&str>,
    ) -> Result<usize, CollabError> {
        // 权限校验（受管房间）。
        if let Some(expected) = self.tokens.lock().await.get(room_id) {
            let ok = token.is_some_and(|t| t == expected);
            if !ok {
                return Err(CollabError::WriteForbidden);
            }
        }
        let sender = {
            let mut rooms = self.rooms.lock().await;
            // 房间不存在时创建（发送方可能从不 join——如只发不收的 host）；
            // 历史优先记录：即使当前无订阅者（全员掉线），帧仍进历史环，重连可找回。
            let room = rooms.entry(room_id.to_string()).or_insert_with(|| Room {
                sender: broadcast::channel(self.capacity).0,
                history: VecDeque::new(),
            });
            room.history.push_back(sealed.clone());
            while room.history.len() > self.replay_limit {
                room.history.pop_front();
            }
            if room.sender.receiver_count() == 0 {
                return Ok(0);
            }
            let sender = room.sender.clone();
            drop(rooms); // 历史写入完成后尽早释放锁
            sender
        };
        Ok(sender.send(sealed).unwrap_or(0))
    }

    /// 注册（或更新）房间的写权限令牌，使房间变为**受管**：无令牌发布被拒。
    ///
    /// 由房间创建者调用（服务端 `/api/collab/room` 生成密钥后注册）；
    /// 令牌仅存于中继内存，房间密钥仍只在分享链接的 `#` 片段。
    pub async fn set_write_token(&self, room_id: &str, token: String) {
        self.tokens.lock().await.insert(room_id.to_string(), token);
    }

    /// 移除房间的写权限令牌（房间回到开放状态）。
    pub async fn clear_write_token(&self, room_id: &str) {
        self.tokens.lock().await.remove(room_id);
    }

    /// 清理所有无订阅者的房间（周期性维护调用，防止长时运行内存增长）。
    ///
    /// 返回被清理的房间数。
    pub async fn cleanup_empty(&self) -> usize {
        let mut rooms = self.rooms.lock().await;
        let before = rooms.len();
        rooms.retain(|_, room| room.sender.receiver_count() > 0);
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
    write_token: Option<String>,
}

impl CollabClient {
    /// 构造：由房间密钥派生不透明 `room_id`。
    ///
    /// 默认无 write token（只读 view 语义）；可写客户端应链式调用
    /// [`with_write_token`](Self::with_write_token) 传入分享链接中的令牌。
    #[must_use]
    pub fn new(relay: Relay, key: RoomKey, client_id: String) -> Self {
        let room_id = room_id(&key);
        Self {
            relay,
            key,
            room_id,
            client_id,
            write_token: None,
        }
    }

    /// 设置写权限令牌（可写分享链接持有；`None` = 只读 view）。
    #[must_use]
    pub fn with_write_token(mut self, token: Option<String>) -> Self {
        self.write_token = token;
        self
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

    /// 订阅本房间并取回历史密封帧（新加入者先补历史，再收实时）。
    pub async fn subscribe_with_replay(&self) -> (Vec<Vec<u8>>, broadcast::Receiver<Vec<u8>>) {
        self.relay.join_with_replay(&self.room_id).await
    }

    /// 密封并发送一帧；`frame` 的 `client_id`/`ts` 由本客户端覆写。
    ///
    /// # Errors
    /// 密封失败时返回 [`CollabError`]；受管房间且本客户端无有效 write token 时返回
    /// [`CollabError::WriteForbidden`]。
    pub async fn send(&self, mut frame: WireFrame) -> Result<usize, CollabError> {
        stamp(&mut frame, &self.client_id);
        let sealed = seal(&self.key, &frame)?;
        self.relay
            .publish_with_token(&self.room_id, sealed, self.write_token.as_deref())
            .await
    }

    /// 解封一条已订阅的密封字节。
    ///
    /// # Errors
    /// 解封失败时返回 [`CollabError`]。
    pub fn decode(&self, sealed: &[u8]) -> Result<WireFrame, CollabError> {
        open(&self.key, sealed)
    }

    /// 解封并做**协议版本校验**（H39）：`Welcome` 携带的 proto 与本端不一致时返回
    /// [`CollabError::ProtoMismatch`]，调用方据此提示用户刷新/升级，而不是带着错误
    /// 假设继续渲染。
    ///
    /// # Errors
    /// 解封失败，或对端协议版本不兼容。
    pub fn decode_checked(&self, sealed: &[u8]) -> Result<WireFrame, CollabError> {
        let frame = open(&self.key, sealed)?;
        if let WireFrame::Welcome { proto, .. } = &frame
            && !crate::frame::proto_compatible(*proto)
        {
            return Err(CollabError::ProtoMismatch {
                remote: *proto,
                local: crate::frame::PROTO_VERSION,
            });
        }
        Ok(frame)
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
        }
        | WireFrame::Hello {
            client_id: c,
            ts: t,
            ..
        }
        | WireFrame::Welcome {
            client_id: c,
            ts: t,
            ..
        }
        | WireFrame::SnapshotChunk {
            client_id: c,
            ts: t,
            ..
        }
        | WireFrame::Bye {
            client_id: c,
            ts: t,
            ..
        }
        | WireFrame::Error {
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
        .map(|d| d.as_millis())
        .ok()
        .and_then(|ms| u64::try_from(ms).ok())
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
    async fn publish_to_empty_room_returns_zero_and_keeps_history() {
        // 回归：向无订阅者的房间 publish 返回 0（无人可送达），但**保留房间与历史**：
        // 全员掉线期间的帧须可被重连找回；空房间由 cleanup_empty 统一回收。
        let relay = Relay::new();
        let room_id = "test-room-2";
        drop(relay.join(room_id).await);
        assert_eq!(relay.room_count().await, 1);
        // publish 到空房间
        let delivered = relay.publish(room_id, vec![1, 2, 3]).await.unwrap();
        assert_eq!(delivered, 0, "无订阅者时不应送达");
        eprintln!("DBG rooms after publish: {}", relay.room_count().await);
        assert_eq!(relay.room_count().await, 1, "空房间保留（历史待重连找回）");
        // 新订阅者仍能取回掉线期间的历史
        let (history, rx) = relay.join_with_replay(room_id).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0], vec![1, 2, 3]);
        drop(rx); // 订阅者退出后，cleanup 才回收
        assert_eq!(relay.cleanup_empty().await, 1);
        assert_eq!(relay.room_count().await, 0);
    }

    #[tokio::test]
    async fn replay_history_to_new_subscriber() {
        // 新订阅者先收到历史密封帧，再收实时帧；历史与实时间无缝隙。
        let relay = Relay::new();
        let key = generate_room_key();
        let a = CollabClient::new(relay.clone(), key, "a".into());
        let b = CollabClient::new(relay.clone(), key, "b".into());

        // 先发 2 条历史
        a.send(WireFrame::Chat {
            client_id: String::new(),
            text: "h1".into(),
            ts: 0,
        })
        .await
        .unwrap();
        a.send(WireFrame::Chat {
            client_id: String::new(),
            text: "h2".into(),
            ts: 0,
        })
        .await
        .unwrap();

        // b 中途加入 → 拿到 2 条历史
        let (history, mut rx) = b.subscribe_with_replay().await;
        assert_eq!(history.len(), 2);
        let texts: Vec<String> = history
            .iter()
            .map(|s| match b.decode(s).unwrap() {
                WireFrame::Chat { text, .. } => text,
                other => panic!("expected Chat, got {other:?}"),
            })
            .collect();
        assert_eq!(texts, vec!["h1", "h2"]);

        // 再发实时帧，顺序在历史之后
        a.send(WireFrame::Chat {
            client_id: String::new(),
            text: "live".into(),
            ts: 0,
        })
        .await
        .unwrap();
        let sealed = rx.recv().await.unwrap();
        match b.decode(&sealed).unwrap() {
            WireFrame::Chat { text, .. } => assert_eq!(text, "live"),
            other => panic!("expected Chat, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn replay_ring_is_bounded() {
        // 历史环截断：超过上限只保留最近 N 条。
        let relay = Relay::with_capacity_and_replay(64, 5);
        let room_id = "bounded-room";
        let rx = relay.join(room_id).await;
        for i in 0..8u8 {
            relay.publish(room_id, vec![i]).await.unwrap();
        }
        drop(rx);
        let (history, _) = relay.join_with_replay(room_id).await;
        assert_eq!(history.len(), 5);
        assert_eq!(history, vec![vec![3], vec![4], vec![5], vec![6], vec![7]]);
    }

    #[tokio::test]
    async fn offline_frames_survive_reconnect() {
        // 全员掉线期间发布的帧记入历史，重连可取回（新增：掉线不丢帧）。
        let relay = Relay::new();
        let room_id = "offline-room";
        drop(relay.join(room_id).await); // 创建房间后全员掉线
        relay.publish(room_id, vec![9]).await.unwrap(); // 掉线期间的帧
        let (history, mut rx) = relay.join_with_replay(room_id).await;
        assert_eq!(history, vec![vec![9]]);
        relay.publish(room_id, vec![10]).await.unwrap();
        let live = rx.recv().await.unwrap();
        assert_eq!(live, vec![10]);
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

    // ── write token 权限（P2-12）──────────────────────────────────────

    #[tokio::test]
    async fn open_room_allows_anonymous_publish() {
        let relay = Relay::new();
        let room = "open-room";
        let _rx = relay.join(room).await;
        let n = relay.publish(room, vec![1]).await.expect("开放房间应可发");
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn managed_room_requires_matching_token() {
        let relay = Relay::new();
        let room = "managed-room";
        relay.set_write_token(room, "tok-abc".into()).await;
        let _rx = relay.join(room).await;
        // 无令牌发布被拒。
        assert!(matches!(
            relay.publish(room, vec![1]).await,
            Err(CollabError::WriteForbidden)
        ));
        // 错误令牌同样被拒。
        assert!(matches!(
            relay.publish_with_token(room, vec![2], Some("wrong")).await,
            Err(CollabError::WriteForbidden)
        ));
        // 正确令牌成功。
        let n = relay
            .publish_with_token(room, vec![3], Some("tok-abc"))
            .await
            .expect("正确令牌应可发");
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn clear_write_token_opens_room_again() {
        let relay = Relay::new();
        let room = "reopen-room";
        relay.set_write_token(room, "t".into()).await;
        relay.clear_write_token(room).await;
        let _rx = relay.join(room).await;
        assert!(relay.publish(room, vec![1]).await.is_ok());
    }

    #[tokio::test]
    async fn client_without_token_cannot_send_to_managed_room() {
        let relay = Relay::new();
        let key = generate_room_key();
        let room = crate::room::room_id(&key);
        relay.set_write_token(&room, "tok-client".into()).await;
        let _rx = relay.join(&room).await;
        // 只读 view 客户端（无 token）。
        let viewer = CollabClient::new(relay.clone(), key, "viewer".into());
        assert!(matches!(
            viewer
                .send(WireFrame::Chat {
                    client_id: String::new(),
                    text: "hi".into(),
                    ts: 0,
                })
                .await,
            Err(CollabError::WriteForbidden)
        ));
        // 可写客户端（带 token）。
        let writer = CollabClient::new(relay, key, "writer".into())
            .with_write_token(Some("tok-client".into()));
        let n = writer
            .send(WireFrame::Chat {
                client_id: String::new(),
                text: "hi".into(),
                ts: 0,
            })
            .await
            .expect("可写客户端应可发");
        assert_eq!(n, 1);
    }

    /// H39：协议版本不匹配时 `decode_checked` 显式报错（而不是让调用方带着错误假设继续）。
    #[tokio::test]
    async fn decode_checked_rejects_incompatible_protocol_version() {
        let relay = Relay::new();
        let key = generate_room_key();
        let host = CollabClient::new(relay.clone(), key, "host".into());
        let guest = CollabClient::new(relay, key, "guest".into());
        let mut rx = guest.subscribe().await;
        // 对端声明 proto=3（omp 当前版本）→ 本端 proto=1 应拒绝。
        let _ = host
            .send(WireFrame::Welcome {
                client_id: "host".into(),
                proto: 3,
                read_only: false,
                entry_count: 0,
                ts: 0,
            })
            .await;
        let sealed = rx.recv().await.expect("应有帧");
        // 普通 decode 仍能解出（诊断/日志路径不受影响）。
        let raw = guest.decode(&sealed).expect("解封应成功");
        assert!(matches!(raw, WireFrame::Welcome { proto: 3, .. }));
        // checked 路径给出结构化错误，消息里含两端版本与可执行建议。
        match guest.decode_checked(&sealed) {
            Err(CollabError::ProtoMismatch { remote, local }) => {
                assert_eq!((remote, local), (3, crate::PROTO_VERSION));
            }
            other => panic!("应报协议不匹配，得到 {other:?}"),
        }
        let msg = crate::proto_mismatch_message(3);
        assert!(msg.contains("proto=3") && msg.contains("proto=1"), "{msg}");

        // 本端版本的 Welcome 正常通过（兼容路径未被误伤）。
        let _ = host
            .send(WireFrame::Welcome {
                client_id: "host".into(),
                proto: crate::PROTO_VERSION,
                read_only: true,
                entry_count: 2,
                ts: 0,
            })
            .await;
        let sealed = rx.recv().await.expect("应有帧");
        assert!(guest.decode_checked(&sealed).is_ok());
        // 非 Welcome 帧不做版本校验（只有握手帧声明版本）。
        let _ = host
            .send(WireFrame::Chat {
                client_id: "host".into(),
                text: "hi".into(),
                ts: 0,
            })
            .await;
        let sealed = rx.recv().await.expect("应有帧");
        assert!(matches!(
            guest.decode_checked(&sealed),
            Ok(WireFrame::Chat { ref text, .. }) if text == "hi"
        ));
    }

    #[test]
    fn proto_compatibility_is_exact_match() {
        assert!(crate::proto_compatible(crate::PROTO_VERSION));
        for other in [0, 2, 3, 99] {
            assert!(!crate::proto_compatible(other), "proto={other} 不应兼容");
        }
    }

    #[tokio::test]
    async fn hello_welcome_frames_roundtrip_through_client() {
        let relay = Relay::new();
        let key = generate_room_key();
        let host = CollabClient::new(relay.clone(), key, "host".into());
        let guest = CollabClient::new(relay, key, "guest".into());
        let mut rx = guest.subscribe().await;
        let _ = host
            .send(WireFrame::Hello {
                client_id: String::new(),
                proto: 1,
                name: Some("host".into()),
                write_token: Some("abc".into()),
                ts: 0,
            })
            .await;
        let sealed = rx.recv().await.expect("应有帧");
        let frame = guest.decode(&sealed).expect("解封");
        match frame {
            WireFrame::Hello {
                proto,
                name,
                write_token,
                ..
            } => {
                assert_eq!(proto, 1);
                assert_eq!(name.as_deref(), Some("host"));
                assert_eq!(write_token.as_deref(), Some("abc"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn generate_write_token_is_hex_of_expected_length() {
        let t = generate_write_token();
        assert_eq!(t.len(), 32, "16 字节 → 32 hex 字符");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, generate_write_token(), "两次生成应不同");
    }
}
