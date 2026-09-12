//! H5：可定制键位系统（action 表 + 配置覆盖）。
//!
//! 移植 oh-my-pi `config/keybindings.ts` 的**声明式 action 表**思路，落到 Gyre 的
//! rustyline 行编辑层：每个 action 有稳定的 wire 名（`app.*`，与 omp 对齐）、默认键、
//! 人类可读说明；用户在配置里用 `[keybindings]` 覆盖：
//!
//! ```toml
//! [keybindings]
//! "app.message.dequeue" = "ctrl-y"
//! "app.clear" = "f5"
//! ```
//!
//! 解析出的键位表在 REPL 启动时一次性绑定到 `rustyline::Editor`；`/hotkeys` 展示
//! **生效**的绑定（而非静态文案）。未知 action / 非法键位只告警不阻断启动。

use std::collections::BTreeMap;

use rustyline::{KeyCode, KeyEvent, Modifiers};

/// 可绑定动作（`wire` 名与 omp `app.*` 对齐）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// 中断当前轮次（Ctrl-C；由信号路由消费，仅登记展示）。
    Interrupt,
    /// 清屏（保留上下文）。
    Clear,
    /// 退出 REPL（EOF）。
    Exit,
    /// 历史反向搜索。
    HistorySearch,
    /// 补全。
    Complete,
    /// 取回队首排队消息到编辑行（H4）。
    Dequeue,
    /// 插入 `/retry` 文本。
    Retry,
    /// 插入 `/new` 文本（新会话）。
    SessionNew,
    /// 插入 `/queue list` 文本。
    QueueList,
    /// 插入 `/settings` 文本。
    Settings,
}

impl Action {
    /// 全部动作（帮助/校验的事实源）。
    pub const ALL: [Self; 10] = [
        Self::Interrupt,
        Self::Clear,
        Self::Exit,
        Self::HistorySearch,
        Self::Complete,
        Self::Dequeue,
        Self::Retry,
        Self::SessionNew,
        Self::QueueList,
        Self::Settings,
    ];

    /// wire 名（配置键）。
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Interrupt => "app.interrupt",
            Self::Clear => "app.clear",
            Self::Exit => "app.exit",
            Self::HistorySearch => "app.history.search",
            Self::Complete => "app.complete",
            Self::Dequeue => "app.message.dequeue",
            Self::Retry => "app.retry",
            Self::SessionNew => "app.session.new",
            Self::QueueList => "app.command.queueList",
            Self::Settings => "app.command.settings",
        }
    }

    /// 默认键位（`None` = 默认不绑定，仅可在配置里显式绑定）。
    #[must_use]
    pub const fn default_key(self) -> Option<&'static str> {
        match self {
            Self::Interrupt => Some("ctrl-c"),
            Self::Clear => Some("ctrl-l"),
            Self::Exit => Some("ctrl-d"),
            Self::HistorySearch => Some("ctrl-r"),
            Self::Complete => Some("tab"),
            Self::Dequeue => Some("ctrl-y"),
            Self::Retry | Self::SessionNew | Self::QueueList | Self::Settings => None,
        }
    }

    /// 绑定后要插入编辑行的命令文本（`None` = 原生编辑行为）。
    #[must_use]
    pub const fn inserts(self) -> Option<&'static str> {
        match self {
            Self::Retry => Some("/retry"),
            Self::SessionNew => Some("/new"),
            Self::QueueList => Some("/queue list"),
            Self::Settings => Some("/settings"),
            _ => None,
        }
    }

    /// 人类可读说明（`/hotkeys` 用）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Interrupt => "中断当前轮次",
            Self::Clear => "清屏（保留上下文）",
            Self::Exit => "退出 REPL",
            Self::HistorySearch => "历史反向搜索",
            Self::Complete => "补全命令/模型/skill",
            Self::Dequeue => "取回排队消息到编辑行",
            Self::Retry => "插入 /retry（重发最近输入）",
            Self::SessionNew => "插入 /new（新会话）",
            Self::QueueList => "插入 /queue list（查看队列）",
            Self::Settings => "插入 /settings（查看设置）",
        }
    }

    /// 按 wire 名查找。
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.wire() == name)
    }
}

/// 一条生效绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// 动作。
    pub action: Action,
    /// 绑定的按键。
    pub key: KeyEvent,
}

/// 键位表解析结果（含诊断，便于启动时告警）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// 生效绑定（按动作表顺序；被覆盖的默认键不再出现）。
    pub bindings: Vec<Binding>,
    /// 未知 action 名（配置笔误；忽略并告警）。
    pub unknown_actions: Vec<String>,
    /// 非法键位（格式错误；忽略并告警）。
    pub invalid_keys: Vec<(String, String)>,
}

/// 从配置映射解析键位表。
///
/// 规则（对齐 omp）：**配置覆盖默认**；同一个键被多个动作争用时**后者胜**（配置遍历
/// 顺序为动作表顺序，最后写入者生效，先前的绑定被移除），避免同键双绑定。
#[must_use]
pub fn resolve(overrides: &BTreeMap<String, String>) -> Resolved {
    let mut out = Resolved::default();
    // 先建「动作 → 键」表：默认键，再被配置覆盖。
    let mut effective: BTreeMap<Action, Option<(KeyEvent, String)>> = BTreeMap::new();
    for action in Action::ALL {
        let configured = overrides.get(action.wire());
        match configured {
            Some(spec) => match parse_key(spec) {
                Ok(key) => {
                    effective.insert(action, Some((key, spec.clone())));
                }
                Err(e) => {
                    out.invalid_keys.push((action.wire().into(), e));
                    // 非法覆盖 → 回退默认键（不因笔误丢掉功能）。
                    if let Some(d) = action.default_key().and_then(|s| parse_key(s).ok()) {
                        effective
                            .insert(action, Some((d, action.default_key().unwrap_or("").into())));
                    }
                }
            },
            None => {
                if let Some(d) = action.default_key().and_then(|s| parse_key(s).ok()) {
                    effective.insert(action, Some((d, action.default_key().unwrap_or("").into())));
                }
            }
        }
    }
    for name in overrides.keys() {
        if Action::from_wire(name).is_none() {
            out.unknown_actions.push(name.clone());
        }
    }
    // 同键冲突：按动作表顺序后者胜（同键只保留最后写入的动作）。
    let mut ordered: Vec<(Action, KeyEvent)> = Vec::new();
    for action in Action::ALL {
        if let Some((key, _)) = effective.get(&action).and_then(Option::as_ref) {
            ordered.retain(|(_, k)| k != key);
            ordered.push((action, *key));
        }
    }
    out.bindings = ordered
        .into_iter()
        .map(|(action, key)| Binding { action, key })
        .collect();
    out
}

/// 解析键位说明：`ctrl-y` / `alt-enter` / `f5` / `tab` / `esc` / 单字符。
///
/// 修饰键可组合（`ctrl-alt-x`），顺序不限；大小写不敏感。
///
/// # Errors
/// 空串、未知修饰键或未知键名时返回可读错误。
pub fn parse_key(spec: &str) -> Result<KeyEvent, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("键位为空".into());
    }
    let mut modifiers = Modifiers::NONE;
    let mut rest = spec;
    // 逐个剥离修饰前缀（`ctrl-alt-x`；顺序不限，未知前缀即键名开始）。
    while let Some((head, tail)) = rest.split_once('-') {
        let modifier = match head.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Modifiers::CTRL,
            "alt" | "meta" | "opt" | "option" => Modifiers::ALT,
            "shift" => Modifiers::SHIFT,
            _ => break,
        };
        modifiers |= modifier;
        rest = tail;
    }
    if rest.is_empty() {
        return Err(format!("键位缺少键名: {spec}"));
    }
    let code = match rest.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        other => {
            if let Some(n) = other
                .strip_prefix('f')
                .and_then(|n| n.parse::<u8>().ok())
                .filter(|n| (1..=12).contains(n))
            {
                KeyCode::F(n)
            } else if rest.chars().count() == 1 {
                KeyCode::Char(rest.chars().next().expect("单字符"))
            } else {
                return Err(format!("未知键名: {spec}"));
            }
        }
    };
    // 与 rustyline 的控制字符归一化一致：`ctrl-<字母>` 统一为**大写** `Char`
    // （否则 `bind_sequence` 绑不到 rustyline 内部的 `Ctrl-Y` 槽位）。
    let code = match (code, modifiers.contains(Modifiers::CTRL)) {
        (KeyCode::Char(c), true) => KeyCode::Char(c.to_ascii_uppercase()),
        (other, _) => other,
    };
    Ok(KeyEvent(code, modifiers))
}

/// 渲染键位（`ctrl-y` / `F5` 等；`/hotkeys` 展示用）。
#[must_use]
pub fn render_key(key: &KeyEvent) -> String {
    let (code, modifiers) = (key.0, key.1);
    let mut parts: Vec<String> = Vec::new();
    if modifiers.contains(Modifiers::CTRL) {
        parts.push("Ctrl".into());
    }
    if modifiers.contains(Modifiers::ALT) {
        parts.push("Alt".into());
    }
    if modifiers.contains(Modifiers::SHIFT) {
        parts.push("Shift".into());
    }
    let base = match code {
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PgUp".into(),
        KeyCode::PageDown => "PgDn".into(),
        other => format!("{other:?}"),
    };
    parts.push(base);
    parts.join("-")
}

/// 渲染生效键位表（`/hotkeys` 输出体）。
#[must_use]
pub fn render_table(resolved: &Resolved) -> Vec<String> {
    let mut lines = Vec::new();
    for b in &resolved.bindings {
        lines.push(format!(
            "  {:<10} {:<34} {}",
            render_key(&b.key),
            b.action.wire(),
            b.action.label()
        ));
    }
    for (action, err) in &resolved.invalid_keys {
        lines.push(format!("  （忽略非法键位 {action}: {err}）"));
    }
    for action in &resolved.unknown_actions {
        lines.push(format!("  （忽略未知动作 {action}）"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_named_keys_and_function_keys() {
        assert_eq!(
            parse_key("ctrl-y").unwrap(),
            KeyEvent(KeyCode::Char('Y'), Modifiers::CTRL),
            "ctrl-<字母> 归一为大写（与 rustyline 控制字符约定一致）"
        );
        assert_eq!(
            parse_key("CTRL-L").unwrap(),
            KeyEvent(KeyCode::Char('L'), Modifiers::CTRL)
        );
        assert_eq!(
            parse_key("alt-enter").unwrap(),
            KeyEvent(KeyCode::Enter, Modifiers::ALT)
        );
        assert_eq!(
            parse_key("f5").unwrap(),
            KeyEvent(KeyCode::F(5), Modifiers::NONE)
        );
        assert_eq!(
            parse_key("tab").unwrap(),
            KeyEvent(KeyCode::Tab, Modifiers::NONE)
        );
        assert_eq!(
            parse_key("esc").unwrap(),
            KeyEvent(KeyCode::Esc, Modifiers::NONE)
        );
        // 组合修饰键（顺序不限）。
        assert_eq!(
            parse_key("alt-ctrl-x").unwrap(),
            KeyEvent(KeyCode::Char('X'), Modifiers::ALT | Modifiers::CTRL)
        );
        // 错误：空串 / 未知键名 / 未知修饰键。
        assert!(parse_key("").is_err());
        assert!(parse_key("hyper-x").is_err());
        assert!(parse_key("f13").is_err());
    }

    #[test]
    fn every_action_has_a_unique_parseable_default() {
        let mut seen: Vec<(KeyEvent, Action)> = Vec::new();
        for action in Action::ALL {
            if let Some(spec) = action.default_key() {
                let key =
                    parse_key(spec).unwrap_or_else(|e| panic!("{} 默认键非法: {e}", action.wire()));
                assert!(!seen.iter().any(|(k, _)| *k == key), "默认键冲突: {spec}");
                seen.push((key, action));
            }
            // wire 名唯一且可反查。
            assert_eq!(Action::from_wire(action.wire()), Some(action));
        }
    }

    #[test]
    fn resolve_applies_overrides_and_reports_bad_input() {
        // 默认表：dequeue 默认 ctrl-y。
        let mut defaults = BTreeMap::new();
        defaults.insert("app.message.dequeue".to_string(), "alt-d".to_string());
        let r = resolve(&defaults);
        let dequeue = r
            .bindings
            .iter()
            .find(|b| b.action == Action::Dequeue)
            .expect("dequeue 应在表中");
        assert_eq!(
            dequeue.key,
            KeyEvent(KeyCode::Char('d'), Modifiers::ALT),
            "配置覆盖默认键"
        );
        // 默认键不应再出现（被移到 alt-d）。
        assert!(
            !r.bindings
                .iter()
                .any(|b| b.key == KeyEvent(KeyCode::Char('Y'), Modifiers::CTRL))
        );
        assert!(r.unknown_actions.is_empty() && r.invalid_keys.is_empty());

        // 非法键位 → 回退默认 + 诊断。
        let mut bad = BTreeMap::new();
        bad.insert("app.clear".to_string(), "hyper-z".to_string());
        let r = resolve(&bad);
        assert_eq!(r.invalid_keys.len(), 1);
        assert!(
            r.bindings.iter().any(|b| b.action == Action::Clear
                && b.key == KeyEvent(KeyCode::Char('L'), Modifiers::CTRL)),
            "非法覆盖应回退默认键"
        );

        // 未知动作 → 忽略并报告。
        let mut unknown = BTreeMap::new();
        unknown.insert("app.nope".to_string(), "ctrl-k".to_string());
        let r = resolve(&unknown);
        assert_eq!(r.unknown_actions, vec!["app.nope".to_string()]);
        // 未绑定动作不出现在表里，但已在配置里绑定的插入型动作会出现。
        unknown.remove("app.nope");
        unknown.insert("app.retry".to_string(), "ctrl-t".to_string());
        let r = resolve(&unknown);
        let retry = r
            .bindings
            .iter()
            .find(|b| b.action == Action::Retry)
            .expect("显式绑定的插入动作应在表中");
        assert_eq!(retry.action.inserts(), Some("/retry"));
    }

    #[test]
    fn duplicate_keys_last_action_wins() {
        // 同一个键同时给 clear 与 dequeue：按动作表顺序后者（dequeue）胜。
        let mut cfg = BTreeMap::new();
        cfg.insert("app.clear".to_string(), "ctrl-y".to_string());
        let r = resolve(&cfg);
        let owners: Vec<Action> = r
            .bindings
            .iter()
            .filter(|b| b.key == KeyEvent(KeyCode::Char('Y'), Modifiers::CTRL))
            .map(|b| b.action)
            .collect();
        assert_eq!(owners, vec![Action::Dequeue], "同键只保留最后写入的动作");
        // clear 失去绑定（不再是 ctrl-l），也不在表里。
        assert!(!r.bindings.iter().any(|b| b.action == Action::Clear));
    }

    #[test]
    fn renders_readable_table_and_keys() {
        let r = resolve(&BTreeMap::new());
        let table = render_table(&r).join("\n");
        assert!(table.contains("Ctrl-Y"), "{table}");
        assert!(table.contains("app.message.dequeue"), "{table}");
        assert!(table.contains("取回排队消息到编辑行"), "{table}");
        assert_eq!(render_key(&KeyEvent(KeyCode::F(5), Modifiers::NONE)), "F5");
        assert_eq!(
            render_key(&KeyEvent(
                KeyCode::Char('A'),
                Modifiers::CTRL | Modifiers::ALT
            )),
            "Ctrl-Alt-A"
        );
    }
}
