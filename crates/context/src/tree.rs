//! 会话树（森林）纯函数工具：路径回溯、分支汇总收集、节点删除、线性日志迁移。
//!
//! 这些函数不持有锁、不触碰 I/O，仅操作 `&[SessionNode]` / `Vec<SessionNode>`，
//! 便于在 [`crate::InMemoryContext`] 的锁内紧凑调用与单元测试。
//!
//! 设计移植 oh-my-pi `compaction/branch-summarization.ts` 的
//! `collectEntriesForBranchSummary` 与 `compaction/entries.ts` 的树形语义：
//! 每个节点带 `parent_id`，多个节点共享同一父即构成分支。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_core::{AgentMessage, NodeId, SessionNode};

/// 生成全局唯一的节点 id：纳秒十六进制 + 进程内单调计数，避免同纳秒并发碰撞。
///
/// 与 [`crate::SessionStore::new_id`] 同源思想，但独立计数器，语义为「节点」。
#[must_use]
pub fn new_node_id() -> NodeId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("n-{nanos:x}-{seq:x}")
}

/// 构建 `id → 节点 vec 下标` 索引。
#[must_use]
pub fn node_index(nodes: &[SessionNode]) -> HashMap<NodeId, usize> {
    nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.clone(), i))
        .collect()
}

/// 按 id 取节点引用。
#[must_use]
pub fn get_node<'a>(nodes: &'a [SessionNode], id: &str) -> Option<&'a SessionNode> {
    node_index(nodes).get(id).map(|&i| &nodes[i])
}

/// 从叶子回溯到根的节点 id 路径（**叶→根**顺序；调用方通常需反转得到根→叶）。
///
/// 对环路 / 缺失父节点做防御：遇到缺失父即停（不应发生，但持久化损坏时保全）。
#[must_use]
pub fn path_to_root(nodes: &[SessionNode], leaf: &str) -> Vec<NodeId> {
    let idx = node_index(nodes);
    let mut out = Vec::new();
    let mut cur = Some(leaf.to_string());
    while let Some(cid) = cur {
        let Some(&i) = idx.get(&cid) else { break };
        out.push(cid.clone());
        cur = nodes[i].parent_id.clone();
        // 环路防御：若回到已访问节点则停（防 parent 成环死循环）。
        if out.iter().filter(|x| *x == &cid).count() > 1 {
            break;
        }
    }
    out
}

/// 从根到叶子的节点 id 路径（**根→叶**顺序）。
#[must_use]
pub fn branch_path_ids(nodes: &[SessionNode], leaf: &str) -> Vec<NodeId> {
    let mut p = path_to_root(nodes, leaf);
    p.reverse();
    p
}

/// 从根到叶子的节点引用序列（**根→叶**顺序，便于直接取 message 组装上下文）。
#[must_use]
pub fn branch_path_nodes<'a>(nodes: &'a [SessionNode], leaf: &str) -> Vec<&'a SessionNode> {
    let idx = node_index(nodes);
    branch_path_ids(nodes, leaf)
        .into_iter()
        .filter_map(|id| idx.get(&id).copied())
        .map(|i| &nodes[i])
        .collect()
}

/// 两叶子的最近公共祖先（deepest common ancestor）；不存在返回 `None`。
#[must_use]
pub fn common_ancestor(nodes: &[SessionNode], a: &str, b: &str) -> Option<NodeId> {
    let path_a: HashSet<NodeId> = path_to_root(nodes, a).into_iter().collect();
    let path_b = branch_path_ids(nodes, b); // 根→叶
    // path_b 根→叶，从深往浅找第一个也在 path_a 中的 → 最近公共祖先。
    for id in path_b.into_iter().rev() {
        if path_a.contains(&id) {
            return Some(id);
        }
    }
    None
}

/// 全部叶子节点（无人以其为父的节点）。
#[must_use]
pub fn leaves(nodes: &[SessionNode]) -> Vec<NodeId> {
    let parents: HashSet<&NodeId> = nodes.iter().filter_map(|n| n.parent_id.as_ref()).collect();
    nodes
        .iter()
        .map(|n| n.id.clone())
        .filter(|id| !parents.contains(id))
        .collect()
}

/// 某节点的直接子节点 id 列表（按 vec 出现顺序）。
#[must_use]
pub fn children_of(nodes: &[SessionNode], id: &str) -> Vec<NodeId> {
    nodes
        .iter()
        .filter(|n| n.parent_id.as_deref() == Some(id))
        .map(|n| n.id.clone())
        .collect()
}

/// 收集「分支切换时应被汇总」的节点：从 `old_leaf` 回溯到与 `target` 的最近公共祖先，
/// 这段**被离开分支独有**的后缀（公共祖先**不含**，old_leaf 含）。
///
/// 移植 oh-my-pi `collectEntriesForBranchSummary`：返回按时间顺序（旧→新）排列的
/// 节点克隆，以及公共祖先 id（供 handoff 摘要挂载定位）。
///
/// - `old_leaf = None`：无被离开分支，返回空。
/// - `old_leaf == target`：无独有后缀，返回空。
#[must_use]
pub fn collect_entries_for_branch_summary(
    nodes: &[SessionNode],
    old_leaf: Option<&str>,
    target: &str,
) -> (Vec<SessionNode>, Option<NodeId>) {
    let Some(old) = old_leaf else {
        return (Vec::new(), None);
    };
    if old == target {
        return (Vec::new(), None);
    }
    let ancestor = common_ancestor(nodes, old, target);
    // 从 old_leaf 向根回溯，直到（不含）公共祖先，收集沿途节点。
    let idx = node_index(nodes);
    let mut entries: Vec<SessionNode> = Vec::new();
    let mut cur: Option<NodeId> = Some(old.to_string());
    while let Some(cid) = cur {
        match idx.get(&cid).copied() {
            Some(i) => {
                let node = &nodes[i];
                if ancestor.as_ref() == Some(&cid) {
                    break;
                }
                entries.push(node.clone());
                cur = node.parent_id.clone();
            }
            None => break,
        }
    }
    entries.reverse(); // 旧 → 新
    (entries, ancestor)
}

/// 删除节点 vec 第 `index` 个节点，并连带清理其孤立的工具结果后代；其余子树
/// （分支）被重新挂到被删节点的父节点上（保枝不丢）。
///
/// 语义对齐旧的 `remove_entry_with_orphans`（删除含工具调用的 assistant 时一并移除
/// 其后继的 ToolResult），但适配树形：被删节点的非工具结果子节点（即真正的分支）
/// 会被重新 parent 到祖父，而不是被删除。
///
/// 返回 `Some(新森林)` 表示发生删除；`None` 表示索引越界。
#[must_use]
pub fn remove_node_with_orphans(
    nodes: &[SessionNode],
    index: usize,
) -> Option<Vec<SessionNode>> {
    if index >= nodes.len() {
        return None;
    }
    let target = &nodes[index];
    let target_parent = target.parent_id.clone();
    // 被删 assistant 的 tool_call_id 集：用于识别需连带删除的 ToolResult 后代。
    let orphan_tool_ids: HashSet<String> = match &target.message {
        AgentMessage::Assistant(a) => a
            .tool_calls()
            .into_iter()
            .map(|(id, _, _)| id.to_string())
            .collect(),
        _ => HashSet::new(),
    };

    // 1) 标记需删除的节点集：target + 其后代链上匹配 orphan_tool_ids 的 ToolResult。
    //    遍历：从 target 出发，沿 parent_id == 已删节点 的链向下找 ToolResult 匹配项。
    let mut removed: HashSet<NodeId> = HashSet::new();
    removed.insert(target.id.clone());
    if !orphan_tool_ids.is_empty() {
        // 简单不动点：反复扫描，把「父在 removed 中、自身为匹配 ToolResult」的节点加入 removed。
        loop {
            let mut grew = false;
            for n in nodes {
                if removed.contains(&n.id) {
                    continue;
                }
                let Some(p) = n.parent_id.as_ref() else { continue };
                if !removed.contains(p) {
                    continue;
                }
                if let AgentMessage::ToolResult(t) = &n.message {
                    if orphan_tool_ids.contains(&t.tool_call_id) {
                        removed.insert(n.id.clone());
                        grew = true;
                    }
                }
            }
            if !grew {
                break;
            }
        }
    }

    // 2) 重建：跳过 removed 节点；其余节点的 parent 若落在 removed 集，则改挂到 target_parent。
    let mut out: Vec<SessionNode> = Vec::with_capacity(nodes.len() - removed.len().min(nodes.len()));
    for n in nodes {
        if removed.contains(&n.id) {
            continue;
        }
        let mut nn = n.clone();
        if let Some(p) = nn.parent_id.as_ref() {
            if removed.contains(p) {
                nn.parent_id = target_parent.clone();
            }
        }
        out.push(nn);
    }
    Some(out)
}

/// 把旧版线性日志（`Vec<AgentMessage>`）无损迁移为单链树（`Vec<SessionNode>`）。
///
/// 每条消息顺序成为节点，`parent_id` 指向前一条（首条为根）。id 采用稳定前缀
/// `legacy-{i}`，便于迁移可复现与排查。
#[must_use]
pub fn wrap_linear_as_nodes(log: &[AgentMessage]) -> Vec<SessionNode> {
    log.iter()
        .enumerate()
        .map(|(i, m)| SessionNode {
            id: format!("legacy-{i}"),
            parent_id: if i == 0 {
                None
            } else {
                Some(format!("legacy-{}", i - 1))
            },
            message: m.clone(),
        })
        .collect()
}

/// 取某叶子所在分支（根→叶）的消息序列（迁移/快照常用）。
#[must_use]
pub fn branch_messages<'a>(nodes: &'a [SessionNode], leaf: &str) -> Vec<&'a AgentMessage> {
    branch_path_nodes(nodes, leaf)
        .into_iter()
        .map(|n| &n.message)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user_text(text)
    }

    /// 构造一条线性链：root → c1 → c2 ...，id 为传入序号。
    fn chain(texts: &[&str]) -> Vec<SessionNode> {
        let mut out = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            out.push(SessionNode {
                id: format!("n{i}"),
                parent_id: if i == 0 { None } else { Some(format!("n{}", i - 1)) },
                message: user(t),
            });
        }
        out
    }

    #[test]
    fn branch_path_root_to_leaf() {
        let nodes = chain(&["a", "b", "c"]);
        assert_eq!(branch_path_ids(&nodes, "n2"), vec!["n0", "n1", "n2"]);
        // 未知叶子 → 空路径。
        assert!(branch_path_ids(&nodes, "zz").is_empty());
    }

    #[test]
    fn common_ancestor_finds_deepest() {
        // root=a, b 子树分叉 b1/b2，b1 下有 c。
        let mut nodes = chain(&["a", "b"]);
        nodes.push(SessionNode {
            id: "b1".into(),
            parent_id: Some("n1".into()),
            message: user("b1"),
        });
        nodes.push(SessionNode {
            id: "b2".into(),
            parent_id: Some("n1".into()),
            message: user("b2"),
        });
        nodes.push(SessionNode {
            id: "c".into(),
            parent_id: Some("b1".into()),
            message: user("c"),
        });
        // c 与 b2 的最近公共祖先是 b（n1）。
        assert_eq!(common_ancestor(&nodes, "c", "b2").as_deref(), Some("n1"));
        // c 与 c 自身 → c。
        assert_eq!(common_ancestor(&nodes, "c", "c").as_deref(), Some("c"));
    }

    #[test]
    fn collect_entries_excludes_common_ancestor() {
        // 分支：a → b → c1 → d1（叶子 d1）；a → b → c2（叶子 c2）。
        let mut nodes = chain(&["a", "b"]);
        nodes.push(SessionNode {
            id: "c1".into(),
            parent_id: Some("n1".into()),
            message: user("c1"),
        });
        nodes.push(SessionNode {
            id: "d1".into(),
            parent_id: Some("c1".into()),
            message: user("d1"),
        });
        nodes.push(SessionNode {
            id: "c2".into(),
            parent_id: Some("n1".into()),
            message: user("c2"),
        });
        // 离开 d1 切到 c2：被汇总的是 d1 路径独有后缀 c1→d1（不含公共祖先 b）。
        let (entries, anc) = collect_entries_for_branch_summary(&nodes, Some("d1"), "c2");
        assert_eq!(anc.as_deref(), Some("n1"));
        let texts: Vec<&str> = entries
            .iter()
            .filter_map(|n| match &n.message {
                AgentMessage::User(u) => u.content.first().and_then(|c| match c {
                    agent_core::UserContent::Text { text } => Some(text.as_str()),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["c1", "d1"]);
    }

    #[test]
    fn leaves_and_children() {
        let mut nodes = chain(&["a", "b"]);
        nodes.push(SessionNode {
            id: "b'".into(),
            parent_id: Some("n0".into()),
            message: user("alt"),
        });
        let leaves_ids: HashSet<String> = leaves(&nodes).into_iter().collect();
        assert_eq!(
            leaves_ids,
            ["n1".to_string(), "b'".to_string()].into_iter().collect(),
            "叶子应为 n1 与 b'"
        );
        // a(n0) 的子：b(n1) 与 b'。
        let ch: HashSet<String> = children_of(&nodes, "n0").into_iter().collect();
        assert_eq!(
            ch,
            ["b'".to_string(), "n1".to_string()].into_iter().collect(),
            "n0 的子应为 b' 与 n1"
        );
    }

    #[test]
    fn wrap_linear_chain_links_parents() {
        let log = vec![user("x"), user("y"), user("z")];
        let nodes = wrap_linear_as_nodes(&log);
        assert_eq!(nodes.len(), 3);
        assert!(nodes[0].parent_id.is_none());
        assert_eq!(nodes[1].parent_id.as_deref(), Some("legacy-0"));
        assert_eq!(nodes[2].parent_id.as_deref(), Some("legacy-1"));
    }

    #[test]
    fn remove_node_preserves_branches() {
        // a → b → c（主线），a → b2（分支）。删除 b(n1)：b2 应改挂到 a(n0)。
        let mut nodes = chain(&["a", "b", "c"]);
        nodes.push(SessionNode {
            id: "b2".into(),
            parent_id: Some("n1".into()),
            message: user("branch"),
        });
        // 删除索引 1（b）。
        let out = remove_node_with_orphans(&nodes, 1).unwrap();
        // b2 仍在，且 parent 改为 a(n0)。
        let b2 = out.iter().find(|n| n.id == "b2").unwrap();
        assert_eq!(b2.parent_id.as_deref(), Some("n0"));
        // b(n1) 被删。
        assert!(out.iter().all(|n| n.id != "n1"));
        // c(n2) 原父 b 被删 → 改挂 a。
        let c = out.iter().find(|n| n.id == "n2").unwrap();
        assert_eq!(c.parent_id.as_deref(), Some("n0"));
    }

    #[test]
    fn remove_node_index_out_of_bounds() {
        let nodes = chain(&["a"]);
        assert!(remove_node_with_orphans(&nodes, 5).is_none());
    }
}
