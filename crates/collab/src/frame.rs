//! Collab 线协议帧（`WireFrame`）：密封前后的明文载荷。
//!
//! 移植自 [`oh-my-pi collab-web`](../../../third/oh-my-pi/packages/collab-web)（浏览器侧 WebCrypto seal/open 的 Rust 对偶）。
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
}

impl WireFrame {
    /// 取发送方客户端 id。
    #[must_use]
    pub fn client_id(&self) -> &str {
        match self {
            Self::Chat { client_id, .. }
            | Self::Tool { client_id, .. }
            | Self::Presence { client_id, .. }
            | Self::Sync { client_id, .. } => client_id,
        }
    }

    /// 取毫秒时间戳。
    #[must_use]
    pub fn ts(&self) -> u64 {
        match self {
            Self::Chat { ts, .. }
            | Self::Tool { ts, .. }
            | Self::Presence { ts, .. }
            | Self::Sync { ts, .. } => *ts,
        }
    }
}
