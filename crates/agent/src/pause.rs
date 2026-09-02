//! 进程级暂停门（移植 oh-my-pi [`AgentPauseGate`](https://github.com/can1357/oh-my-pi/blob/master/packages/agent/src/pause.ts)）。
//!
//! 每一个 agent loop —— 主会话、子代理（task tool）、advisor —— 都在 [`crate::Agent::run`]
//! 的两个安全点轮询此门：**每次 provider 调用前**与**每次工具批执行前**。pause 时，在途
//! provider 流和已启动的工具执行跑完后 park（冻结），随后每个 loop 停在安全点直到
//! [`PauseGate::resume`]；queued steering / follow-up 消息保持排队，resume 后正常投递。
//!
//! run 自身的 [`CancellationToken`](tokio_util::sync::CancellationToken) 仍能立即解除 park：
//! park 在 [`PauseGate::wait_until_resumed`] 内用 `tokio::select!` 同时等待 resume 与 cancel，
//! cancel 优先（biased）——故取消单个 run 永远无需 resume 整个进程。
//!
//! Host（CLI/Web）驱动单例 [`PauseGate`]（如 `/pause` 命令）；库代码只读。
//!
//! # 为什么在 `agent` crate 而非 `agent-core`
//! [`agent-core`](agent_core) 是零具体实现依赖的契约层（不含 tokio）；PauseGate 依赖
//! `tokio::sync::watch`，属运行时机制，故置于本 crate。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// 进程级暂停门。见[模块文档](self)。
#[derive(Debug)]
pub struct PauseGate {
    /// 当前是否暂停：`false` = 运行，`true` = 暂停。`watch` 保留最后值，新 parker 立即见到当前状态。
    tx: watch::Sender<bool>,
    /// 本次暂停开始时刻（运行时为 `None`）。
    paused_at: std::sync::Mutex<Option<Instant>>,
}

impl PauseGate {
    /// 构造运行态（未暂停）的门，返回 `Arc` 便于多 loop 共享 + host 驱动。
    #[must_use]
    pub fn new() -> Arc<Self> {
        let (tx, _rx) = watch::channel(false);
        Arc::new(Self {
            tx,
            paused_at: std::sync::Mutex::new(None),
        })
    }

    /// 暂停（冻结所有 loop 于下一安全点）。已暂停时返回 `false`（无操作）。
    ///
    /// 用 `send_modify` 而非 `send`：watch 的 `send` 在无活 receiver 时返回 `Err` 且**不更新**
    /// 值（[`PauseGate::new`] 的内部 `_rx` 在返回后即 drop），`send_modify` 总是修改并通知。
    pub fn pause(&self) -> bool {
        let mut was_paused = false;
        self.tx.send_modify(|v| {
            was_paused = *v;
            *v = true;
        });
        if was_paused {
            false
        } else {
            *self.paused_at.lock().expect("pause_at poisoned") = Some(Instant::now());
            true
        }
    }

    /// 恢复所有 parked loop，返回本次暂停时长（未暂停时返回 `None`）。
    pub fn resume(&self) -> Option<Duration> {
        let mut was_paused = false;
        self.tx.send_modify(|v| {
            was_paused = *v;
            *v = false;
        });
        if was_paused {
            self.paused_at
                .lock()
                .expect("pause_at poisoned")
                .take()
                .map(|t| t.elapsed())
        } else {
            None
        }
    }

    /// 当前是否暂停。
    #[must_use]
    pub fn paused(&self) -> bool {
        *self.tx.borrow()
    }

    /// 若暂停则 park 至 `resume` 或 `cancel`；未暂停立即返回。
    ///
    /// `cancel` 立即解除 park（**不**释放门）——取消单个 run 无需 resume 整个进程，
    /// 对齐 oh-my-pi「park 在 abort 时释放但不释放 gate」语义。
    pub async fn wait_until_resumed(&self, cancel: &CancellationToken) {
        if !self.paused() {
            return;
        }
        let mut rx = self.tx.subscribe();
        // biased：cancel 优先于 resume（取消语义优先）。
        tokio::select! {
            biased;
            () = cancel.cancelled() => {}
            () = wait_until_false(&mut rx) => {}
        }
    }
}

/// 等待 watch 值变为 `false`（resume）。已是 `false` 立即返回；否则阻塞到值变更。
async fn wait_until_false(rx: &mut watch::Receiver<bool>) {
    while *rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            // 发送端 dropped（PauseGate 被销毁）——视同 resume，解除 park。
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn park_returns_immediately_when_running() {
        // 未暂停时 wait_until_resumed 立即返回（不阻塞）。
        let gate = PauseGate::new();
        let cancel = CancellationToken::new();
        let elapsed = tokio::time::Instant::now();
        gate.wait_until_resumed(&cancel).await;
        assert!(
            elapsed.elapsed() < std::time::Duration::from_millis(50),
            "未暂停时应立即返回"
        );
    }

    #[tokio::test]
    async fn park_blocks_until_resume() {
        // 暂停后 wait_until_resumed 阻塞，直到 resume。
        let gate = PauseGate::new();
        let cancel = CancellationToken::new();
        assert!(gate.pause());
        assert!(gate.paused());

        let gate_clone = Arc::clone(&gate);
        let handle = tokio::spawn(async move {
            gate_clone.wait_until_resumed(&cancel).await;
        });

        // 此时 task 应仍 parked。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!handle.is_finished(), "暂停时应 park 阻塞");

        let dur = gate.resume();
        assert!(dur.is_some(), "resume 应返回暂停时长");
        assert!(!gate.paused());

        // resume 后 park 解除，task 完成。
        handle.await.expect("task panicked");
    }

    #[tokio::test]
    async fn cancel_releases_park_without_resume() {
        // 暂停时 cancel 立即解除 park（不释放门），故 paused() 仍为 true。
        let gate = PauseGate::new();
        let cancel = CancellationToken::new();
        gate.pause();

        let gate_clone = Arc::clone(&gate);
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            gate_clone.wait_until_resumed(&cancel_clone).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!handle.is_finished(), "暂停时应 park 阻塞");

        cancel.cancel();
        handle.await.expect("task panicked");

        // 关键：cancel 解除 park 但不释放门——paused 仍 true。
        assert!(
            gate.paused(),
            "cancel 解除 park 但不释放门（paused 仍 true）"
        );
    }

    #[tokio::test]
    async fn pause_is_idempotent() {
        // 重复 pause 返回 false（无操作）；resume 后 paused_at 清空。
        let gate = PauseGate::new();
        assert!(gate.pause(), "首次 pause 返回 true");
        assert!(!gate.pause(), "重复 pause 返回 false");
        assert!(gate.resume().is_some());
        assert!(gate.resume().is_none(), "未暂停时 resume 返回 None");
    }
}
