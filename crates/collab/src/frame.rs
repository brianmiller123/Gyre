//! Collab 线协议帧（`WireFrame`）：密封前后的明文载荷。
//!
//! 移植自 [`oh-my-pi collab-web`](../../../third/oh-my-pi/packages/collab-web)（浏览器侧 `WebCrypto` seal/open 的 Rust 对偶）。
//! 覆盖协同会话所需的最小消息集：聊天、工具活动、在线状态、状态快照同步。

use serde::{Deserialize, Serialize};

/// 协同会话线协议帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireFrame {
    /// 聊天消息。
    Chat {
        /// 发送方客户端 id。
        client_id: String,
        /// 正文。
        text: String,
        /// 毫秒时间戳。
        ts: u64,
    },
    /// 工具活动（执行/结果广播）。
    Tool {
        /// 发送方客户端 id。
        client_id: String,
        /// 工具名。
        tool: String,
        /// 参数（JSON）。
        args: serde_json::Value,
        /// 结果（可选；执行中为 None）。
        result: Option<serde_json::Value>,
        /// 毫秒时间戳。
        ts: u64,
    },
    /// 在线状态。
    Presence {
        /// 客户端 id。
        client_id: String,
        /// 显示名。
        display: Option<String>,
        /// 上线/下线。
        online: bool,
        /// 毫秒时间戳。
        ts: u64,
    },
    /// 状态快照同步（后加入者拉取/新人补齐）。
    Sync {
        /// 发送方客户端 id。
        client_id: String,
        /// 快照载荷（由应用定义，如序列化会话日志）。
        snapshot: String,
        /// 毫秒时间戳。
        ts: u64,
    },
    /// 握手帧（P2-12）：客户端声明协议版本、显示名与写权限令牌。
    ///
    /// Gyre 全密封形态：hello 与其余帧一样经 AES-GCM 密封，中继保持盲视；
    /// `write_token` 供应用层（host 侧）裁决读写权限——中继层权限走连接元数据
    /// （见 [`Relay::set_write_token`](crate::relay::Relay::set_write_token)）。
    Hello {
        /// 发送方客户端 id。
        client_id: String,
        /// 线协议版本（当前 `1`）。
        proto: u32,
        /// 显示名。
        name: Option<String>,
        /// 写权限令牌（base64url；缺省 = 只读 view 链接）。
        write_token: Option<String>,
        /// 毫秒时间戳。
        ts: u64,
    },
    /// 欢迎帧（P2-12）：协议版本握手 + 只读标记 + 后续快照块总数。
    ///
    /// Gyre 广播对等模型下由房间发起方（host 侧）宣告；`entry_count` 与
    /// [`SnapshotChunk`](Self::SnapshotChunk) 的 `seq` 配合使接收端在收齐前保持加载态。
    Welcome {
        /// 发送方客户端 id。
        client_id: String,
        /// 线协议版本。
        proto: u32,
        /// 本 peer 是否只读（未持 write token 加入）。
        read_only: bool,
        /// 快照分块总数（`seq` 0..`entry_count`；`final_chunk` 帧提前到达亦可收尾）。
        entry_count: usize,
        /// 毫秒时间戳。
        ts: u64,
    },
    /// 快照分块（P2-12）：把超大会话切成字节有界的帧，避免单帧压垮中继/WS 帧上限。
    SnapshotChunk {
        /// 发送方客户端 id。
        client_id: String,
        /// 块序号（从 0 起严格递增；乱序/重复被聚合器拒绝）。
        seq: u32,
        /// 是否为最后一块（收尾信号；即使 `entry_count` 未到亦可完成聚合）。
        final_chunk: bool,
        /// 块载荷（UTF-8 边界安全切割）。
        chunk: String,
        /// 毫秒时间戳。
        ts: u64,
    },
    /// 优雅离开（P2-12）。
    Bye {
        /// 发送方客户端 id。
        client_id: String,
        /// 离开原因（如 `user-left` / `host-closed`）。
        reason: String,
        /// 毫秒时间戳。
        ts: u64,
    },
    /// 协议错误（P2-12）：版本不匹配 / 权限拒绝 / 乱序分块等。
    Error {
        /// 发送方客户端 id。
        client_id: String,
        /// 错误描述。
        message: String,
        /// 毫秒时间戳。
        ts: u64,
    },
}

impl WireFrame {
    /// 取发送方客户端 id。
    #[must_use]
    pub fn client_id(&self) -> &str {
        match self {
            Self::Chat { client_id, .. }
            | Self::Tool { client_id, .. }
            | Self::Presence { client_id, .. }
            | Self::Sync { client_id, .. }
            | Self::Hello { client_id, .. }
            | Self::Welcome { client_id, .. }
            | Self::SnapshotChunk { client_id, .. }
            | Self::Bye { client_id, .. }
            | Self::Error { client_id, .. } => client_id,
        }
    }

    /// 取毫秒时间戳。
    #[must_use]
    pub const fn ts(&self) -> u64 {
        match self {
            Self::Chat { ts, .. }
            | Self::Tool { ts, .. }
            | Self::Presence { ts, .. }
            | Self::Sync { ts, .. }
            | Self::Hello { ts, .. }
            | Self::Welcome { ts, .. }
            | Self::SnapshotChunk { ts, .. }
            | Self::Bye { ts, .. }
            | Self::Error { ts, .. } => *ts,
        }
    }
}
