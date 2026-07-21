//! 子 Agent 监控仪表盘（终端备用屏 + 自动刷新）。
//!
//! # 交互设计（回答「切换视图不破坏当前输入」）
//!
//! - **进入备用屏** `\x1b[?1049h`：主屏（含 rustyline 提示符与未提交输入）原样沉在底层，
//!   备用屏与主屏是终端的两套独立缓冲（同 `vim`/`tmux`/`less` 机制）。
//! - **刷新**：`interval(125ms)` 重绘整个备用屏，光标归位 `\x1b[H` + 行尾 `\x1b[K` + 末尾 `\x1b[J`。
//!   所有字节只写进备用屏，主屏零写入 → 输入行纹丝不动。
//! - **退出**：用户按 `Enter`（后台 `read_line` 触发取消）→ 写 `\x1b[?1049l` 回到进入前精确位置；
//!   rustyline 的输入缓冲在其内部状态（与终端屏无关），原样保留；该 Enter 被后台消费，不泄漏成空任务。
//!
//! # 演进
//!
//! 当前「Enter 返回」为零新依赖的稳健 MVP。若要 ↑/↓/Tab 单键导航 + 日志全屏钻取，
//! 需 raw 模式读单键（推荐 `crossterm`：`enable_raw_mode()` + `event::read()`），
//! 与本模块的 `render_loop` 正交，可作为独立增强落地（见 `plans/subagent-monitor.md` §8.4）。

use std::io::Write;
use std::time::Duration;

use agent_supervisor::{SubAgentPhase, SubAgentStatus, Supervisor};

/// i18n 取词宏。
use agent_i18n::t;

const REFRESH_MS: u64 = 125;

/// 运行监控仪表盘，直到用户按 Enter 返回。
///
/// 直接读取同一进程的 [`Supervisor`] 快照（CLI 与 TaskTool 共享一份状态）。
///
/// # Errors
///
/// 终端写入失败时返回错误（极罕见）。
pub async fn run_dashboard(supervisor: &Supervisor) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout();
    // 进入备用屏 + 清屏 + 光标归位 + 隐藏光标
    write!(stdout, "\x1b[?1049h\x1b[2J\x1b[H\x1b[?25l")?;
    stdout.flush()?;

    // 监听 Enter 退出的后台任务（行缓冲读一行）
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_for_input = cancel.clone();
    let input_task = tokio::task::spawn_blocking(move || {
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);
        cancel_for_input.cancel();
    });

    render_loop(supervisor, &cancel).await;

    // 恢复光标 + 退出备用屏（终端回到进入前精确位置）
    write!(stdout, "\x1b[?25h\x1b[?1049l")?;
    stdout.flush()?;
    input_task.abort();
    Ok(())
}

/// 自动刷新循环：每 `REFRESH_MS` 重绘一次，直到 `cancel` 触发。
async fn render_loop(supervisor: &Supervisor, cancel: &tokio_util::sync::CancellationToken) {
    let mut stdout = std::io::stdout();
    let mut ticker = tokio::time::interval(Duration::from_millis(REFRESH_MS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let agents = supervisor.snapshot().await;
                let frame = render_frame(&agents);
                if write!(stdout, "\x1b[H{frame}").is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
        }
    }
}

/// 渲染整屏（汇总 + 逐卡片）。
#[allow(clippy::too_many_lines)]
fn render_frame(agents: &[SubAgentStatus]) -> String {
    let mut out = String::new();
    out.push_str("╭─ 子 Agent 监控 ──────────────── 按 Enter 返回 ─╮\x1b[K\n");

    if agents.is_empty() {
        out.push_str("│  （当前无运行中的子 Agent）\x1b[K\n");
    } else {
        let running = agents.iter().filter(|a| !a.phase.is_terminal()).count();
        let done = agents
            .iter()
            .filter(|a| a.phase == SubAgentPhase::Done)
            .count();
        let failed = agents
            .iter()
            .filter(|a| a.phase == SubAgentPhase::Failed)
            .count();
        let total_tokens: u64 = agents
            .iter()
            .map(|a| a.usage.input_tokens + a.usage.output_tokens)
            .sum();
        out.push_str(&format!(
            "│  {active} \x1b[1m{running}\x1b[0m · {done_w} \x1b[32m{done}\x1b[0m · {failed_w} \x1b[31m{failed}\x1b[0m · {total_w} {total_tokens} tokens\x1b[K\n",
            active = t!("agents.active"),
            done_w = t!("agents.done"),
            failed_w = t!("agents.failed"),
            total_w = t!("agents.total"),
        ));
        out.push_str("├────────────────────────────────────────────────\x1b[K\n");
        for a in agents {
            out.push_str(&render_card(a));
        }
    }
    out.push_str("╰────────────────────────────────────────────────╯\x1b[J\n");
    out
}

/// 渲染单个子 Agent 卡片（每行末尾 `\x1b[K` 清至行尾，避免残留）。
fn render_card(a: &SubAgentStatus) -> String {
    let phase = phase_tag(a.phase);
    let bar = progress_bar(a.progress);
    let tokens = a.usage.input_tokens + a.usage.output_tokens;
    let cost = if a.usage.cost_usd > 0.0 {
        format!(" ${:.4}", a.usage.cost_usd)
    } else {
        String::new()
    };
    let mut s = format!("│ {phase} {} {bar}\x1b[K\n", ellipsis(&a.label, 34));
    s.push_str(&format!(
        "│   {}: {}\x1b[K\n",
        t!("agents.task"),
        ellipsis(&a.task, 52)
    ));
    s.push_str(&format!(
        "│   {} {} · {} {} · tokens {tokens}{cost}",
        t!("agents.turns"),
        a.turns,
        t!("agents.tools"),
        a.tool_calls
    ));
    if let Some(act) = &a.current_activity {
        s.push_str(&format!(" · {act}"));
    }
    s.push_str("\x1b[K\n");
    if let Some(last) = a.logs.last() {
        s.push_str(&format!("│   ▸ {}\x1b[K\n", ellipsis(&last.text, 60)));
    }
    if let Some(e) = &a.error {
        s.push_str(&format!("│   \x1b[31m✗ {}\x1b[0m\x1b[K\n", ellipsis(e, 60)));
    }
    s
}

/// 进度条 `[████░░░░░░] 42%`。
fn progress_bar(p: f32) -> String {
    let p = p.clamp(0.0, 1.0);
    let width = 10usize;
    let pct = (p * 100.0).round() as u32;
    let filled = ((p * width as f32).round() as usize).min(width);
    let bar: String = "█".repeat(filled) + &"░".repeat(width - filled);
    format!("[{bar}] {pct:>3}%")
}

/// 阶段彩色图标 + 中文标签。
fn phase_tag(p: SubAgentPhase) -> String {
    let (icon, color, label) = match p {
        SubAgentPhase::Pending => ("·", "90", t!("agents.phase.pending")),
        SubAgentPhase::Running => ("▶", "36", t!("agents.phase.running")),
        SubAgentPhase::Streaming => ("~", "34", t!("agents.phase.streaming")),
        SubAgentPhase::WaitingTool => ("⏸", "33", t!("agents.phase.waiting_tool")),
        SubAgentPhase::Done => ("✓", "32", t!("agents.phase.done")),
        SubAgentPhase::Failed => ("✗", "31", t!("agents.phase.failed")),
        SubAgentPhase::Cancelled => ("⊘", "90", t!("agents.phase.cancelled")),
    };
    format!("\x1b[{color}m{icon}{label}\x1b[0m")
}

/// 截断到 `max` 字符（超长加 …），换行折叠为空格。
fn ellipsis(s: &str, max: usize) -> String {
    let t = s.trim().replace('\n', " ");
    if t.chars().count() <= max {
        t
    } else {
        let mut o: String = t.chars().take(max.saturating_sub(1)).collect();
        o.push('…');
        o
    }
}
