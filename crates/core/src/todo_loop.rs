//! H28：待办循环端口（eager prelude 与完成提醒续跑的只读数据源）。
//!
//! 引擎（`agent`）在**首轮**注入「先规划再动手」的 prelude、在**停止边界**对未完成
//! 待办注入提醒并续跑。这两处都需要读待办清单，但清单实体（`agent-tools::TodoState`）
//! 位于工具层——引擎不能反向依赖工具 crate，故在此定义端口，工具层实现它，
//! 装配层（cli/server/rpc）把同一实例同时交给 `TodoTool` 与 `Agent`。
//!
//! 对齐 oh-my-pi `session/todo-tracker.ts`（`TodoTracker` 从 `SessionManager` 分支
//! 取待办），Gyre 侧以 `Arc<dyn TodoLoopSource>` 取代会话管理器：同一份清单、
//! 同一生命周期，无第二份状态。

/// 待办单条的只读视图（与工具层 `TodoItem` 解耦：状态用线协议名，便于提醒渲染）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoLoopEntry {
    /// 任务内容（短语）。
    pub content: String,
    /// 线协议状态名（`pending` / `in_progress` / `blocked` / `completed` / `abandoned`）。
    pub phase: String,
    /// 阻塞原因（仅 `blocked` 有意义）。
    pub blocker: Option<String>,
}

/// 待办清单快照（循环侧只读；`incomplete` 为未完成条数，0 表示已清空）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoLoopSnapshot {
    /// 全部条目（含已完成历史，供「是否规划过」判定）。
    pub entries: Vec<TodoLoopEntry>,
    /// 未完成条数（`pending` / `in_progress` / `blocked`）。
    pub incomplete: usize,
}

impl TodoLoopSnapshot {
    /// 是否尚未规划过任何待办（eager prelude 的触发条件之一）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 是否存在未完成待办（完成提醒的触发条件）。
    #[must_use]
    pub const fn has_incomplete(&self) -> bool {
        self.incomplete > 0
    }

    /// 渲染未完成条目为缩进列表；无未完成返回 `None`。
    #[must_use]
    pub fn render_incomplete(&self) -> Option<String> {
        let lines: Vec<String> = self
            .entries
            .iter()
            .filter(|e| !is_done(&e.phase))
            .map(|e| {
                let blocker = e
                    .blocker
                    .as_deref()
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map_or_else(String::new, |b| format!("（阻塞：{b}）"));
                format!("- [{}] {}{}", e.phase, e.content, blocker)
            })
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }
}

/// 状态名是否为「已结束」（完成或放弃）。
#[must_use]
pub fn is_done(phase: &str) -> bool {
    matches!(phase.trim(), "completed" | "abandoned")
}

/// 待办清单只读源（引擎每轮停止边界调用 `snapshot`）。
pub trait TodoLoopSource: Send + Sync {
    /// 当前清单快照（实现须内部加锁并防御性克隆）。
    fn snapshot(&self) -> TodoLoopSnapshot;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(content: &str, phase: &str) -> TodoLoopEntry {
        TodoLoopEntry {
            content: content.into(),
            phase: phase.into(),
            blocker: None,
        }
    }

    #[test]
    fn render_incomplete_skips_done_entries() {
        let snap = TodoLoopSnapshot {
            entries: vec![
                entry("写测试", "pending"),
                entry("读代码", "completed"),
                entry("改文档", "abandoned"),
            ],
            incomplete: 1,
        };
        // 已完成/已放弃不进入提醒列表；未完成按清单顺序渲染。
        assert_eq!(
            snap.render_incomplete().as_deref(),
            Some("- [pending] 写测试")
        );
        assert!(snap.has_incomplete());
        assert!(!snap.is_empty());
    }

    #[test]
    fn render_incomplete_includes_blocker_and_returns_none_when_clear() {
        let blocked = TodoLoopSnapshot {
            entries: vec![TodoLoopEntry {
                content: "等上游".into(),
                phase: "blocked".into(),
                blocker: Some(" 缺 token ".into()),
            }],
            incomplete: 1,
        };
        assert_eq!(
            blocked.render_incomplete().as_deref(),
            Some("- [blocked] 等上游（阻塞：缺 token）")
        );
        let clear = TodoLoopSnapshot {
            entries: vec![entry("用完", "completed")],
            incomplete: 0,
        };
        assert!(clear.render_incomplete().is_none(), "无未完成 → 不渲染提醒");
        assert!(!clear.has_incomplete());
    }
}
