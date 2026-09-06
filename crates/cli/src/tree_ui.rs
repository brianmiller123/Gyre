//! 会话树 UI 渲染（纯函数，无 I/O / 无锁）。
//!
//! 供 REPL `/tree` 命令与 RPC 控制消息共用：把 [`agent_core::SessionNode`] 森林
//! 渲染为缩进文本或 JSON 数组，并把用户输入的节点目标（完整 id 或唯一前缀）解析为
//! 节点 id。多根森林按插入顺序渲染；父缺失的孤儿节点防御性按根处理（持久化损坏时
//! 不丢行）。
//!
//! # 活跃路径推导：provider 消息跟随 `active_leaf`（已验证跟随，无需修复）
//!
//! - [`agent_context::InMemoryContext::build_provider_context`]（crates/context/src/lib.rs
//!   `impl ContextManager`，第 457 行）取 `inner.active_path_messages()` 组装 provider 消息；
//! - `active_path_messages`（同文件 90–98 行）经 `tree::branch_path_nodes(&self.nodes, leaf)`
//!   （crates/context/src/tree.rs 74–83 行，**根→叶**顺序）从 `active_leaf` 回溯；
//! - `append_node`（lib.rs 305–319 行）以当前活跃叶子为 parent 并前移续写点；
//!   `set_active_leaf`（lib.rs 328–338 行）纯移动续写点并清空稳定前缀。
//!
//! 因此 `set_active_leaf` 切换后，下一轮 LLM 请求只含新活跃路径的消息。回归证据见本
//! 模块测试 [`tests::fork_append_lands_under_active_leaf_and_provider_follows`]（真实
//! `InMemoryContext`：append → 切叶 → append → `build_provider_context` 断言消息恰为
//! 新路径）。

use std::collections::{HashMap, HashSet};

use agent_core::{AgentMessage, SessionNode, UserContent};

/// 预览最大字符数（超长截断加省略号，字符边界安全）。
const PREVIEW_CHARS: usize = 40;

/// 行首 id 展示长度（`[前 8 位]`）。
const ID_PREFIX_CHARS: usize = 8;

/// 前缀匹配的最少输入字符数（低于此值不猜 id，避免误跳分支）。
const MIN_PREFIX_CHARS: usize = 4;

/// 渲染整棵会话树为多行文本（每行以 `\n` 结尾）。
///
/// 行格式：`<缩进>[{id 前 8 位}] <glyph> <角色> <预览>`；活跃节点行尾追加 ` ◀`。
/// 缩进每层 2 空格；glyph：user `>`、assistant `<`、tool_result `=`、其余 `·`。
/// 角色命名与 rpc.rs `message_preview` 一致（user/assistant/tool_result/status/…）。
#[must_use]
pub fn render_tree(nodes: &[SessionNode], active: Option<&str>) -> String {
    let mut out = String::new();
    for (depth, node) in ordered_depths(nodes) {
        let (role, preview) = role_and_preview(&node.message);
        for _ in 0..depth {
            out.push_str("  ");
        }
        out.push('[');
        out.push_str(id_head(&node.id));
        out.push_str("] ");
        out.push(glyph_of(role));
        out.push(' ');
        out.push_str(role);
        out.push(' ');
        out.push_str(&preview_of(&preview));
        if active.is_some_and(|a| a == node.id) {
            out.push_str(" ◀");
        }
        out.push('\n');
    }
    out
}

/// 渲染会话树为 JSON 数组（按渲染顺序 DFS 根→叶）。
///
/// 元素形状（camelCase 面，与既有 RPC/ACP 一致）：
/// `[{"id":完整id,"parent":null|id,"role":"...","preview":"...","active":bool}]`。
#[must_use]
pub fn tree_json(nodes: &[SessionNode], active: Option<&str>) -> serde_json::Value {
    let items: Vec<serde_json::Value> = ordered_depths(nodes)
        .into_iter()
        .map(|(_, node)| {
            let (role, preview) = role_and_preview(&node.message);
            serde_json::json!({
                "id": node.id,
                "parent": node.parent_id,
                "role": role,
                "preview": preview_of(&preview),
                "active": active.is_some_and(|a| a == node.id),
            })
        })
        .collect();
    serde_json::Value::Array(items)
}

/// 解析节点目标：完整 id 精确命中，或 ≥4 字符的前缀唯一匹配。
///
/// 需要节点列表做前缀唯一性判定（任务签名省略了该参数，此处为可实现的完整形态）。
/// 歧义（多个命中）、无命中、输入过短（<4 字符）或空白均返回 `None`；
/// 完整 id 精确命中不受最短长度限制。
#[must_use]
pub fn parse_node_target(input: &str, nodes: &[SessionNode]) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    // 完整 id 精确命中优先（用户通常从 tree_json 复制完整 id）。
    if let Some(hit) = nodes.iter().find(|n| n.id == input) {
        return Some(hit.id.clone());
    }
    if input.chars().count() < MIN_PREFIX_CHARS {
        return None;
    }
    let mut matched: Option<&str> = None;
    for n in nodes {
        if n.id.starts_with(input) {
            if matched.is_some() {
                return None; // 歧义：多个前缀命中
            }
            matched = Some(&n.id);
        }
    }
    matched.map(str::to_string)
}

/// 渲染顺序（DFS 根→叶，兄弟按插入顺序）：`(缩进深度, 节点)` 序列。
///
/// 根 = `parent_id` 为 `None` 或父不在森林中（孤儿防御）。每个节点在 children
/// 映射中至多一个入口（parent 唯一），栈式 DFS 不会重复访问同一节点。
fn ordered_depths(nodes: &[SessionNode]) -> Vec<(usize, &SessionNode)> {
    let ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let mut children: HashMap<&str, Vec<&SessionNode>> = HashMap::new();
    let mut roots: Vec<&SessionNode> = Vec::new();
    for n in nodes {
        match n.parent_id.as_deref() {
            Some(p) if ids.contains(p) => children.entry(p).or_default().push(n),
            _ => roots.push(n),
        }
    }
    let mut out: Vec<(usize, &SessionNode)> = Vec::with_capacity(nodes.len());
    let mut stack: Vec<(usize, &SessionNode)> = roots.into_iter().rev().map(|n| (0, n)).collect();
    while let Some((depth, node)) = stack.pop() {
        out.push((depth, node));
        if let Some(kids) = children.get(node.id.as_str()) {
            // 逆序入栈 → 出栈顺序与插入顺序一致。
            for kid in kids.iter().rev() {
                stack.push((depth + 1, kid));
            }
        }
    }
    out
}

/// 角色与预览原文（截断/折叠见 [`preview_of`]）。角色命名对齐 rpc.rs `message_preview`。
fn role_and_preview(msg: &AgentMessage) -> (&'static str, String) {
    match msg {
        AgentMessage::User(u) => (
            "user",
            u.content
                .iter()
                .filter_map(|c| match c {
                    UserContent::Text { text } => Some(text.as_str()),
                    UserContent::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        ),
        AgentMessage::Assistant(a) => {
            let text = a.text();
            let text = if text.is_empty() && a.has_tool_calls() {
                let names: Vec<&str> = a.tool_calls().iter().map(|(_, name, _)| *name).collect();
                format!("[tool_call: {}]", names.join(", "))
            } else {
                text
            };
            ("assistant", text)
        }
        AgentMessage::ToolResult(t) => ("tool_result", t.result.to_llm_text()),
        AgentMessage::Status(s) => ("status", s.text.clone()),
        AgentMessage::Ask(a) => ("ask", a.prompt.clone()),
        AgentMessage::SoftRequirement(r) => ("soft_requirement", r.reminder.clone()),
    }
}

/// 预览整理：折叠控制字符（换行/制表）为空格（渲染一行一节点），截断到
/// [`PREVIEW_CHARS`] 字符并追加省略号（字符边界安全）。
fn preview_of(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if flat.chars().count() <= PREVIEW_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(PREVIEW_CHARS).collect();
    cut.push('…');
    cut
}

fn glyph_of(role: &str) -> char {
    match role {
        "user" => '>',
        "assistant" => '<',
        "tool_result" => '=',
        _ => '·',
    }
}

/// id 展示头（前 [`ID_PREFIX_CHARS`] 字符；更短则原样）。
fn id_head(id: &str) -> &str {
    id.get(..ID_PREFIX_CHARS).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{
        Api, AssistantMessage, ContentBlock, Model, ProviderMessage, ToolResult, ToolResultMessage,
        Usage,
    };

    /// 手工构造节点（id 形如 `aaaaaaaa11111111`，便于断言 8 字符截取与前缀匹配）。
    fn node(id: &str, parent: Option<&str>, msg: AgentMessage) -> SessionNode {
        SessionNode {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            message: msg,
        }
    }

    fn assistant_text(text: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        })
    }

    fn assistant_tool_call(name: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: "call1".into(),
                name: name.into(),
                arguments: serde_json::json!({ "path": "x" }),
                signature: None,
            }],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: None,
            stop_details: None,
        })
    }

    fn tool_result_text(text: &str) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "call1".into(),
            result: ToolResult::Text(text.into()),
        })
    }

    #[test]
    fn render_marks_active_and_indents_depth() {
        let nodes = vec![
            node(
                "aaaaaaaa11111111",
                None,
                AgentMessage::user_text("你好，世界"),
            ),
            node(
                "bbbbbbbb22222222",
                Some("aaaaaaaa11111111"),
                assistant_text("这是回答"),
            ),
            node(
                "cccccccc33333333",
                Some("bbbbbbbb22222222"),
                tool_result_text("42"),
            ),
            node(
                "dddddddd44444444",
                Some("aaaaaaaa11111111"),
                assistant_tool_call("read_file"),
            ),
            node(
                "eeeeeeee55555555",
                Some("cccccccc33333333"),
                assistant_text("深层"),
            ),
        ];
        let out = render_tree(&nodes, Some("cccccccc33333333"));
        let lines: Vec<&str> = out.lines().collect();
        // DFS 根→叶：a、b、c、e（c 的子）、d（a 的第二子）。
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "[aaaaaaaa] > user 你好，世界");
        assert_eq!(lines[1], "  [bbbbbbbb] < assistant 这是回答");
        assert_eq!(lines[2], "    [cccccccc] = tool_result 42 ◀");
        assert_eq!(lines[3], "      [eeeeeeee] < assistant 深层");
        assert_eq!(lines[4], "  [dddddddd] < assistant [tool_call: read_file]");
    }

    #[test]
    fn render_folds_newlines_and_truncates_preview() {
        // 60 字符 + 换行：换行折叠为空格，超 40 字符截断加 …。
        let nodes = vec![node(
            "aaaaaaaa11111111",
            None,
            AgentMessage::user_text(format!("第一行\n第二行{}", "ab".repeat(30))),
        )];
        let line = render_tree(&nodes, None)
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(line.contains("第一行 第二行"));
        assert!(line.ends_with('…'));
        let prefix_len = "[aaaaaaaa] > user ".chars().count();
        assert_eq!(line.chars().count(), prefix_len + PREVIEW_CHARS + 1);
    }

    #[test]
    fn tree_json_shape_and_active_flag() {
        let nodes = vec![
            node("aaaaaaaa11111111", None, AgentMessage::user_text("q")),
            node(
                "bbbbbbbb22222222",
                Some("aaaaaaaa11111111"),
                assistant_text("a"),
            ),
            node(
                "cccccccc33333333",
                Some("aaaaaaaa11111111"),
                tool_result_text("r"),
            ),
        ];
        let v = tree_json(&nodes, Some("bbbbbbbb22222222"));
        let arr = v.as_array().expect("应为 JSON 数组");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["id"], "aaaaaaaa11111111");
        assert!(arr[0]["parent"].is_null(), "根节点 parent 应为 null");
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["preview"], "q");
        assert_eq!(arr[0]["active"], false);
        assert_eq!(arr[1]["parent"], "aaaaaaaa11111111");
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[1]["active"], true);
        assert_eq!(arr[2]["role"], "tool_result");
        assert_eq!(arr[2]["active"], false);
        // 键集合稳定（camelCase 面；serde_json 默认按键排序存储）。
        let mut keys: Vec<&str> = arr[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["active", "id", "parent", "preview", "role"]);
    }

    #[test]
    fn parse_target_prefix_rules() {
        let nodes = vec![
            node("aaaa1111zz", None, AgentMessage::user_text("1")),
            node("aaaa2222zz", None, AgentMessage::user_text("2")),
            node("bbbb3333zz", None, AgentMessage::user_text("3")),
        ];
        // 完整 id 精确命中。
        assert_eq!(
            parse_node_target("bbbb3333zz", &nodes).as_deref(),
            Some("bbbb3333zz")
        );
        // 前缀唯一（≥4 字符）。
        assert_eq!(
            parse_node_target("aaaa1", &nodes).as_deref(),
            Some("aaaa1111zz")
        );
        assert_eq!(
            parse_node_target("bbbb", &nodes).as_deref(),
            Some("bbbb3333zz")
        );
        // 前缀歧义 → None。
        assert_eq!(parse_node_target("aaaa", &nodes), None);
        // 过短（<4 字符）即使唯一也拒绝。
        assert_eq!(parse_node_target("bbb", &nodes), None);
        // 无命中 / 空白。
        assert_eq!(parse_node_target("ffff", &nodes), None);
        assert_eq!(parse_node_target("  ", &nodes), None);
    }

    /// 真实 `InMemoryContext`：分叉后 append 落在新活跃叶子下；provider 消息
    /// 只含新活跃路径（根→叶）—— `active_leaf` 跟随的证据测试。
    #[tokio::test]
    async fn fork_append_lands_under_active_leaf_and_provider_follows() {
        use agent_context::InMemoryContext;
        use agent_core::ContextManager as _;

        let ctx = InMemoryContext::new(vec!["sys".into()]);
        let a = ctx.append_node(AgentMessage::user_text("q1")).await;
        let b = ctx.append_node(assistant_text("a1")).await;
        assert_eq!(ctx.snapshot().await.len(), 2, "初始活跃路径 a→b");
        assert_eq!(ctx.active_leaf().await.as_deref(), Some(b.id.as_str()));

        // 切回 a 后 append：新节点父为 a（fork），不挂在旧叶 b。
        assert!(ctx.set_active_leaf(&a.id).await);
        let c = ctx.append_node(AgentMessage::user_text("q2-alt")).await;
        assert_eq!(c.parent_id.as_deref(), Some(a.id.as_str()));
        let d = ctx.append_node(assistant_text("a2-alt")).await;
        assert_eq!(d.parent_id.as_deref(), Some(c.id.as_str()));

        // snapshot_nodes 断言全林父子关系（旧分支 b 保全）。
        let nodes = ctx.snapshot_nodes().await;
        let parent_of = |id: &str| {
            nodes
                .iter()
                .find(|n| n.id == id)
                .and_then(|n| n.parent_id.clone())
        };
        assert_eq!(parent_of(&b.id).as_deref(), Some(a.id.as_str()));
        assert_eq!(parent_of(&c.id).as_deref(), Some(a.id.as_str()));
        assert_eq!(parent_of(&d.id).as_deref(), Some(c.id.as_str()));

        // provider 消息恰为新活跃路径 a→c→d（UI 消息被过滤后 3 条）。
        let model = Model::with_defaults("m", "openai", Api::OpenAiCompletions);
        let built = ctx.build_provider_context(&model, &[]).await.unwrap();
        let texts: Vec<String> = built.messages.iter().map(msg_text).collect();
        assert_eq!(texts, ["q1", "q2-alt", "a2-alt"]);

        // 渲染整林：活跃标记落在 d（当前活跃叶子）。
        let out = render_tree(&nodes, Some(&d.id));
        assert!(out.contains(&format!("[{}] < assistant a2-alt ◀", &d.id[..8])));
    }

    /// 提取 provider 消息文本（断言用）。
    fn msg_text(m: &ProviderMessage) -> String {
        match m {
            ProviderMessage::User { content } => content
                .iter()
                .filter_map(|c| match c {
                    UserContent::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
            ProviderMessage::Assistant { content } => content
                .iter()
                .filter_map(|b| b.as_text().map(str::to_string))
                .collect::<Vec<_>>()
                .join(""),
            ProviderMessage::Tool { content, .. } => content.clone(),
            ProviderMessage::System(s) => s.clone(),
        }
    }
}
