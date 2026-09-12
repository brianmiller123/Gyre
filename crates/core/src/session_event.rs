//! 会话级结构化事件（移植 oh-my-pi `packages/coding-agent/src/session/agent-session-events.ts`，
//! `AgentSessionEvent` 的可映射子集）。
//!
//! 与 [`AgentEvent::Say`](crate::AgentEvent::Say) 构成**双通道**：
//!
//! - `Say`（[`StatusMessage`]）承载自由文本 —— 展示层与既有消费者继续可用；
//! - [`SessionEvent`] 承载结构化字段 —— server 帧 / RPC / 前端状态机无需解析文案。
//!
//! 二者在同一点成对发射，展示文本由结构化事件派生（[`SessionEvent::to_status`]），
//! 因而不会出现「文案改了、结构没改」的漂移。上游对应关系：
//! `auto_compaction_start` / `auto_compaction_end` / `auto_retry_start` /
//! `auto_retry_end` / `retry_fallback_applied` / `retry_fallback_succeeded`。
//!
//! 未映射的上游事件（`todo_reminder` / `thinking_level_changed` / `goal_updated` /
//! `config_warnings_changed` 等）在 Gyre 尚无对应运行时，故不设变体（避免空壳）。

use serde::{Deserialize, Serialize};

use crate::hook::HookEvent;
use crate::message::{StatusKind, StatusMessage};

impl SessionEvent {
    /// 转成 hook 事件（H20）：事件名即 serde `type` 判别字段（`auto_compaction_start`
    /// 等），负载为事件本体的扁平 JSON——一个转换点覆盖全部会话级事件，新增变体无需改引擎。
    #[must_use]
    pub fn to_hook_event(&self) -> HookEvent {
        let payload = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        let name = payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("session_event")
            .to_string();
        // 经 `named()` 构造：负载补上 `event` 字段，hook 脚本读到的事件形状与
        // `HookEvent::to_payload()` 的其余变体一致（`{"event": …, ...字段}`）。
        HookEvent::named(name, payload)
    }
}

/// 自动压缩诱因（对齐 omp `auto_compaction_start.reason`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionReason {
    /// 上下文逼近模型上限（阈值触发）。
    Threshold,
    /// 上游返回上下文溢出。
    Overflow,
    /// 空闲维护压缩。
    Idle,
    /// 回复不完整后的收尾压缩。
    Incomplete,
}

/// 压缩动作（对齐 omp `auto_compaction_*.action`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionAction {
    /// 上下文满维护（shake → summarize/prune 级联）。
    ContextFull,
    /// 仅 shake（归档可重取的工具结果）。
    Shake,
    /// snapcompact（本地 PNG 帧替代 LLM 摘要）。
    Snapcompact,
    /// 会话交接压缩。
    Handoff,
}

/// 已执行的压缩级（按执行顺序记录于 [`SessionEvent::AutoCompactionEnd::stages`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionStage {
    /// 归档可重取的工具结果区段。
    Shake,
    /// LLM 摘要折叠历史。
    Summarize,
    /// 本地 PNG 帧压缩（snapcompact 后端）。
    Snapcompact,
    /// 裁剪最旧消息。
    Prune,
}

impl CompactionStage {
    /// 稳定展示名（与 serde 表示一致，供展示文本拼接）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shake => "shake",
            Self::Summarize => "summarize",
            Self::Snapcompact => "snapcompact",
            Self::Prune => "prune",
        }
    }
}

/// 会话级结构化事件。序列化为带 `type` 判别字段的扁平对象
/// （与 omp wire 形态一致，如 `{"type":"auto_retry_start", ...}`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// 自动压缩开始。
    AutoCompactionStart {
        /// 诱因。
        reason: CompactionReason,
        /// 动作。
        action: CompactionAction,
    },
    /// 自动压缩结束。
    AutoCompactionEnd {
        /// 动作。
        action: CompactionAction,
        /// 实际执行的级（按顺序；开始事件与结束事件之间完成）。
        stages: Vec<CompactionStage>,
        /// 各级是否全部成功。
        ok: bool,
        /// 失败原因（`ok == false` 时有值）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// 自动重试开始（携带触发重试的错误与退避时长）。
    AutoRetryStart {
        /// 第几次重试（1-based）。
        attempt: u32,
        /// 重试预算（最多重试次数，不含首次尝试；对齐 omp `maxAttempts`）。
        max_attempts: u32,
        /// 退避等待毫秒数。
        delay_ms: u64,
        /// 触发重试的错误文本。
        error_message: String,
        /// 触及重试的模型 id。
        model: String,
    },
    /// 自动重试结束（成功或放弃）。
    AutoRetryEnd {
        /// 重试后是否成功。
        success: bool,
        /// 已执行的重试次数。
        attempt: u32,
        /// 终局错误（`success == false` 时有值）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    /// 重试回退到备用模型（主模型可重试失败后切换）。
    RetryFallbackApplied {
        /// 起（失败）模型 id。
        from: String,
        /// 止（即将尝试）模型 id。
        to: String,
    },
    /// 备用模型调用成功。
    RetryFallbackSucceeded {
        /// 成功模型 id。
        model: String,
    },
    /// 目标状态变更（H29）：`goal` 工具或引擎（续跑/预算/中断）改了状态。
    ///
    /// `status` 为线协议名（`active` / `paused` / `budget-limited` / `complete` / `dropped`）；
    /// `objective` 为目标首行（已终结或无目标时缺省）；`continuations` 为已续跑轮数。
    GoalUpdated {
        /// 目标状态（线协议名）。
        status: String,
        /// 目标首行。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        objective: Option<String>,
        /// 已续跑轮数。
        continuations: usize,
    },
}

impl SessionEvent {
    /// 展示通道投影（双通道同源）：返回应随本事件成对发射的 [`StatusMessage`]。
    ///
    /// `None` 表示该事件仅供状态机消费（开始的加载态、成功结束的清理信号、
    /// 终局失败已有 [`AgentEvent::Error`](crate::AgentEvent::Error) 兜底），
    /// 不产生额外用户可见文本。
    #[must_use]
    pub fn to_status(&self) -> Option<StatusMessage> {
        let (text, kind) = match self {
            Self::AutoCompactionStart { .. } | Self::AutoRetryEnd { .. } => return None,
            Self::AutoCompactionEnd {
                stages, ok, error, ..
            } => {
                if *ok {
                    let joined = stages
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" + ");
                    (
                        format!("上下文接近上限，已触发压缩（{joined}）"),
                        StatusKind::Warning,
                    )
                } else {
                    (
                        format!("压缩未能完成（{}）", error.as_deref().unwrap_or("未知原因")),
                        StatusKind::Warning,
                    )
                }
            }
            Self::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                model,
                ..
            } => (
                format!(
                    "429 速率限制：{delay_ms} ms 后重试 {model}（尝试 {attempt}/{max_attempts}）"
                ),
                StatusKind::Warning,
            ),
            Self::RetryFallbackApplied { from, to } => {
                (format!("模型回退：{from} → {to}"), StatusKind::Warning)
            }
            Self::RetryFallbackSucceeded { model } => {
                (format!("回退模型 {model} 调用成功"), StatusKind::Info)
            }
            Self::GoalUpdated {
                status,
                objective,
                continuations,
            } => {
                let who = objective.as_deref().unwrap_or("（无目标）");
                (
                    format!("目标已更新：{who} → {status}（续跑 {continuations}）"),
                    StatusKind::Info,
                )
            }
        };
        Some(StatusMessage { text, kind })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 形态：`type` 判别字段 + kebab-case 枚举（与 omp `AgentSessionEvent` 对齐）。
    #[test]
    fn serde_wire_shape_matches_omp() {
        let ev = SessionEvent::AutoCompactionStart {
            reason: CompactionReason::Threshold,
            action: CompactionAction::ContextFull,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "auto_compaction_start");
        assert_eq!(v["reason"], "threshold");
        assert_eq!(v["action"], "context-full");

        let ev = SessionEvent::AutoRetryStart {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 1500,
            error_message: "429".into(),
            model: "claude".into(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "auto_retry_start");
        assert_eq!(v["attempt"], 2);
        assert_eq!(v["delay_ms"], 1500);

        // 往返：省略的可选字段反序列化回 None。
        let back: SessionEvent = serde_json::from_value(serde_json::json!({
            "type": "auto_retry_end", "success": false, "attempt": 3
        }))
        .unwrap();
        assert_eq!(
            back,
            SessionEvent::AutoRetryEnd {
                success: false,
                attempt: 3,
                final_error: None
            }
        );
    }

    /// 双通道同源：展示文本由结构化字段派生，与既有自由文本逐字一致。
    #[test]
    fn to_status_derives_display_text() {
        let retry = SessionEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "upstream 429".into(),
            model: "m1".into(),
        };
        let s = retry.to_status().unwrap();
        assert_eq!(s.text, "429 速率限制：2000 ms 后重试 m1（尝试 1/3）");
        assert_eq!(s.kind, StatusKind::Warning);

        let compact = SessionEvent::AutoCompactionEnd {
            action: CompactionAction::ContextFull,
            stages: vec![CompactionStage::Shake, CompactionStage::Summarize],
            ok: true,
            error: None,
        };
        assert_eq!(
            compact.to_status().unwrap().text,
            "上下文接近上限，已触发压缩（shake + summarize）"
        );

        // 开始的加载态与成功结束的清理信号无展示文本（成功线由 fallback/压缩结束承担）。
        assert!(
            SessionEvent::AutoCompactionStart {
                reason: CompactionReason::Threshold,
                action: CompactionAction::ContextFull,
            }
            .to_status()
            .is_none()
        );
        assert!(
            SessionEvent::AutoRetryEnd {
                success: true,
                attempt: 2,
                final_error: None,
            }
            .to_status()
            .is_none()
        );

        let failed = SessionEvent::AutoCompactionEnd {
            action: CompactionAction::ContextFull,
            stages: vec![CompactionStage::Shake],
            ok: false,
            error: Some("摘要器不可用".into()),
        };
        let s = failed.to_status().unwrap();
        assert!(s.text.contains("摘要器不可用"), "{}", s.text);
    }
}
