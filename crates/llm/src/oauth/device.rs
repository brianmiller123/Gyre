//! RFC 8628 设备码流轮询器（omp `device-code.ts` + github-copilot 倍率策略）。
//!
//! 轮询策略：每轮实际等待 `interval × 1.2`；`slow_down` 优先用服务器 interval
//! 字段（秒），否则 +5s，并把倍率升到 1.4；`authorization_pending` 继续；
//! 其他失败立即抛；deadline 超时报错（提示时钟漂移可能性，与 omp 一致）。

use std::time::{Duration, Instant};

fn timeout_message(slowed: bool) -> String {
    let hint = if slowed {
        "（曾收到 slow_down：若本地时钟正常，检查 WSL/虚拟机时钟漂移）"
    } else {
        ""
    };
    format!("设备码已过期{hint}。请重新发起登录。")
}

/// 轮询最小间隔（omp `MINIMUM_DEVICE_FLOW_INTERVAL_MS`）。
const MIN_INTERVAL_MS: u64 = 1000;
/// 常规倍率（omp `INITIAL_POLL_INTERVAL_MULTIPLIER`）。
const BASE_MULTIPLIER: f64 = 1.2;
/// slow_down 后倍率（omp `SLOW_DOWN_POLL_INTERVAL_MULTIPLIER`）。
const SLOWED_MULTIPLIER: f64 = 1.4;
/// slow_down 无服务器 interval 时的增量（omp `SLOW_DOWN_INTERVAL_INCREMENT_MS`）。
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

/// 单次轮询结果（flow 的 poll 闭包产出）。
pub enum Poll<T> {
    /// 拿到最终产物（token 等）。
    Complete(T),
    /// 用户尚未授权，继续轮询。
    Pending,
    /// 服务器要求放慢：携带服务器建议的 interval（秒，可选）。
    SlowDown(Option<u64>),
    /// 确定性失败（invalid_grant 等），立即终止。
    Failed(String),
}

/// 轮询配置（测试注入小间隔）。
#[derive(Debug, Clone)]
pub struct DevicePollConfig {
    /// 服务器给的初始 interval（秒）；内部 clamp 到 ≥1s。
    pub initial_interval: Duration,
    /// 总 deadline（一般是 expires_in）。
    pub deadline: Instant,
}

impl DevicePollConfig {
    #[must_use]
    pub fn new(initial_secs: u64, expires_in: Duration) -> Self {
        Self {
            initial_interval: Duration::from_secs(initial_secs.max(1)),
            deadline: Instant::now() + expires_in,
        }
    }
}

/// 通用设备码轮询。`poll` 闭包做一次 HTTP 轮询（interval 入参供打点/日志）。
///
/// # Errors
/// `Failed` 态、deadline 超时或 poll 闭包自身 IO 错误向上传播。
pub async fn poll_device_flow<T, F, Fut>(cfg: DevicePollConfig, mut poll: F) -> anyhow::Result<T>
where
    F: FnMut(Duration) -> Fut,
    Fut: Future<Output = anyhow::Result<Poll<T>>>,
{
    let mut interval = cfg
        .initial_interval
        .max(Duration::from_millis(MIN_INTERVAL_MS));
    let mut slowed = false;
    loop {
        if Instant::now() >= cfg.deadline {
            anyhow::bail!("{}", timeout_message(slowed));
        }
        match poll(interval).await? {
            Poll::Complete(v) => return Ok(v),
            Poll::Pending => {}
            Poll::SlowDown(server_interval) => {
                interval = server_interval
                    .map(|s| Duration::from_secs(s.max(1)))
                    .unwrap_or(interval + SLOW_DOWN_INCREMENT);
                slowed = true;
            }
            Poll::Failed(msg) => anyhow::bail!("{msg}"),
        }
        let wait = interval.mul_f64(if slowed {
            SLOWED_MULTIPLIER
        } else {
            BASE_MULTIPLIER
        });
        if Instant::now() + wait > cfg.deadline {
            anyhow::bail!("{}", timeout_message(slowed));
        }
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_cfg() -> DevicePollConfig {
        DevicePollConfig {
            initial_interval: Duration::from_millis(1),
            deadline: Instant::now() + Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn pending_then_complete() {
        let n = std::cell::Cell::new(0);
        let out: String = poll_device_flow(tiny_cfg(), |_| {
            let n = &n;
            async move {
                n.set(n.get() + 1);
                if n.get() < 3 {
                    Ok(Poll::Pending)
                } else {
                    Ok(Poll::Complete("tok".into()))
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(out, "tok");
        assert_eq!(n.get(), 3);
    }

    #[tokio::test]
    async fn slow_down_grows_interval_and_failed_aborts() {
        let polls = std::cell::Cell::new(0);
        let start = Instant::now();
        let cfg = DevicePollConfig {
            initial_interval: Duration::from_millis(1),
            deadline: Instant::now() + Duration::from_secs(30),
        };
        let out: Result<String, anyhow::Error> = poll_device_flow(cfg, |_| {
            let polls = &polls;
            async move {
                polls.set(polls.get() + 1);
                match polls.get() {
                    1 => Ok(Poll::SlowDown(Some(1))),
                    2 => Ok(Poll::Pending),
                    _ => Ok(Poll::Failed("invalid_grant".into())),
                }
            }
        })
        .await;
        let err = out.unwrap_err();
        assert!(err.to_string().contains("invalid_grant"), "{err}");
        // 第 1 轮 slow_down 后按 max(1s)*1.4 睡了至少 ~1.4s 才轮到第 3 次失败。
        assert!(
            start.elapsed() >= Duration::from_millis(1300),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn deadline_expires_with_clock_hint() {
        let cfg = DevicePollConfig {
            initial_interval: Duration::from_millis(1),
            deadline: Instant::now() + Duration::from_millis(50),
        };
        let out: Result<String, anyhow::Error> =
            poll_device_flow(cfg, |_| async { Ok(Poll::SlowDown(None)) }).await;
        let err = out.unwrap_err().to_string();
        assert!(err.contains("时钟漂移"), "{err}");
    }
}
