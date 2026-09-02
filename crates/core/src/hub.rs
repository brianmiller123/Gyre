//! 进程级消息总线（移植 oh-my-pi `irc/bus` 的进程内子集 + hub 邮箱语义）。
//!
//! 多 Agent 协作的最小可信原语：**注册表 + 定向邮箱**。
//! - [`Hub::register`]：注册一个代理 id，返回其专属收件箱（`mpsc::UnboundedReceiver`）。
//! - [`Hub::send`]：向指定 id 投递消息（接收方尚未注册 → 明确报错，不静默丢弃）。
//! - [`Hub::peers`]：列出全部在册 id（供模型发现可通信对象）。
//! - [`Hub::unregister`]：注销（drop 收件箱）。
//!
//! v1 边界（文档化）：仅进程内、无持久化、无广播、无后台任务句柄面
//! （后台任务/复活语义由 supervisor + task 体系承载，见 `agent-supervisor`）。
//! 与 omp `irc/bus.ts` 的差距：跨进程 IPC、回执（ACK）、唤醒/复活协议留待后续。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// 消息投递错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HubError {
    /// 目标未注册（或已注销）。
    RecipientNotFound(String),
    /// 收件箱已关闭（接收方已退出）。
    RecipientGone(String),
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecipientNotFound(id) => write!(f, "收件人 {id:?} 未注册"),
            Self::RecipientGone(id) => write!(f, "收件人 {id:?} 收件箱已关闭"),
        }
    }
}

/// 一条总线消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubMessage {
    /// 发送方 id。
    pub from: String,
    /// 消息体（纯文本；结构化载荷由调用方自行编码，如 JSON 字符串）。
    pub body: String,
}

/// 进程级消息总线（`Arc<Hub>` 共享；线程安全）。
pub struct Hub {
    peers: RwLock<HashMap<String, tokio::sync::mpsc::UnboundedSender<HubMessage>>>,
}

impl Hub {
    /// 构造空总线。
    #[must_use]
    pub fn new() -> Self {
        Self {
            peers: RwLock::new(HashMap::new()),
        }
    }

    /// 共享句柄。
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// 注册代理，返回其收件箱（重名注册以新代旧，旧收件箱关闭）。
    pub fn register(&self, id: String) -> tokio::sync::mpsc::UnboundedReceiver<HubMessage> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut g = self
            .peers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.insert(id, tx);
        rx
    }

    /// 注销代理（重复注销幂等）。
    pub fn unregister(&self, id: &str) {
        let mut g = self
            .peers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.remove(id);
    }

    /// 向指定代理投递消息。
    ///
    /// # Errors
    /// 目标未注册或收件箱已关闭时返回 [`HubError`]。
    pub fn send(&self, to: &str, from: &str, body: impl Into<String>) -> Result<(), HubError> {
        let tx = {
            let g = self
                .peers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.get(to)
                .cloned()
                .ok_or_else(|| HubError::RecipientNotFound(to.to_string()))?
        };
        tx.send(HubMessage {
            from: from.to_string(),
            body: body.into(),
        })
        .map_err(|_| HubError::RecipientGone(to.to_string()))
    }

    /// 全部在册代理 id（排序输出，供展示）。
    #[must_use]
    pub fn peers(&self) -> Vec<String> {
        let g = self
            .peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ids: Vec<String> = g.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// 在册代理数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_send_recv_unregister() {
        let hub = Hub::new().shared();
        let mut rx = hub.register("alpha".into());
        hub.register("beta".into());
        assert_eq!(hub.peers(), vec!["alpha".to_string(), "beta".to_string()]);

        hub.send("alpha", "main", "你好").unwrap();
        hub.send("alpha", "beta", "第二封").unwrap();
        // 未注册目标报错。
        assert_eq!(
            hub.send("nobody", "main", "x"),
            Err(HubError::RecipientNotFound("nobody".into()))
        );

        let m1 = rx.blocking_recv().unwrap();
        assert_eq!(m1.from, "main");
        assert_eq!(m1.body, "你好");
        let m2 = rx.blocking_recv().unwrap();
        assert_eq!(m2.from, "beta");
        assert_eq!(m2.body, "第二封");

        hub.unregister("alpha");
        hub.unregister("alpha"); // 幂等
        assert_eq!(hub.peers(), vec!["beta".to_string()]);
        // 注销后投递 → 未注册错误。
        assert_eq!(
            hub.send("alpha", "main", "x"),
            Err(HubError::RecipientNotFound("alpha".into()))
        );
    }

    #[test]
    fn reregister_replaces_inbox() {
        let hub = Hub::new().shared();
        let mut rx1 = hub.register("a".into());
        // 重名注册：旧收件箱关闭（新收件箱存活才能收到消息）。
        let _rx2 = hub.register("a".into());
        assert!(hub.send("a", "x", "1").is_ok());
        assert!(rx1.try_recv().is_err(), "旧收件箱应被替换");
    }

    #[tokio::test]
    async fn send_after_drop_reports_gone() {
        let hub = Hub::new().shared();
        {
            let _rx = hub.register("t".into());
        }
        // receiver drop → unbounded_sender.send 报 RecipientGone。
        match hub.send("t", "x", "1") {
            Err(HubError::RecipientGone(id)) => assert_eq!(id, "t"),
            other => panic!("期望 RecipientGone，得到 {other:?}"),
        }
    }
}
