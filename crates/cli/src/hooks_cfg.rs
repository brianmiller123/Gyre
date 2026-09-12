//! 配置驱动的 shell 钩子（TOML `[[hooks]]`）：把 [`agent_config::HookRule`] 装配为
//! [`agent_core::Hook`] 实现。
//!
//! 语义：
//! - `before_tool`：spawn `sh -c <command>`（Windows `cmd /C`；`GYRE_SHELL` 可覆盖），
//!   stdin 写事件 JSON `{"event":"before_tool","tool":"<名>","args":<原始参数 JSON>}`，
//!   读 stdout 首行 JSON——`{"decision":"deny","reason":"..."}` → 经
//!   [`agent_core::Hook::before_tool_intercept`] 拦截工具执行；`{"decision":"allow"}`、
//!   解析失败或超时 → 放行。
//! - `after_tool` / `stop`：通知型，同样触发命令但**不解析决定**（`on_event` 通道）。
//! - 超时 kill 子进程；spawn 失败按放行处理并 `tracing::warn`。
//!
//! 本模块为纯功能层：不依赖 cli 装配层（main/repl/rpc），可独立测试。

use std::sync::Arc;
use std::time::Duration;

use agent_config::{HookEventKind, HookRule};
use agent_core::{Hook, HookEvent};
use serde::Deserialize;
use serde_json::json;

/// 单条 shell 钩子规则的运行时实现。
#[derive(Debug, Clone)]
pub struct ShellHook {
    rule: HookRule,
}

impl ShellHook {
    /// 由配置规则构造。
    #[must_use]
    pub fn new(rule: HookRule) -> Self {
        Self { rule }
    }

    /// 跨平台构造子进程 shell 命令（`GYRE_SHELL` 可覆盖；对齐 `agent_tools::shell` 的做法）。
    fn shell_command(command: &str) -> tokio::process::Command {
        #[cfg(unix)]
        {
            let shell = std::env::var("GYRE_SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            let mut c = tokio::process::Command::new(shell);
            c.arg("-c").arg(command);
            c
        }
        #[cfg(windows)]
        {
            let shell = std::env::var("GYRE_SHELL").unwrap_or_else(|_| "cmd".to_string());
            let mut c = tokio::process::Command::new(shell);
            c.arg("/C").arg(command);
            c
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = command;
            compile_error!("ShellHook 仅支持 unix 与 windows");
        }
    }

    /// 执行命令：stdin 写 `payload`，读 stdout 首行（超时 kill）。
    /// 返回 `None` 表示 spawn 失败 / EOF / 读取失败 / 超时（均按放行或忽略处理）。
    async fn run_first_line(&self, payload: &str) -> Option<String> {
        let mut child = match Self::shell_command(&self.rule.command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    command = %self.rule.command,
                    error = %e,
                    "shell hook spawn 失败，按放行处理"
                );
                return None;
            }
        };
        // stdin 写入独立任务：命令可以不读 stdin（如 echo），BrokenPipe 一律忽略。
        if let Some(mut stdin) = child.stdin.take() {
            let payload = payload.to_string();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(payload.as_bytes()).await;
                let _ = stdin.shutdown().await;
            });
        }
        use tokio::io::AsyncBufReadExt;
        let mut stdout = tokio::io::BufReader::new(child.stdout.take()?);
        let mut line = String::new();
        let timeout = Duration::from_secs(self.rule.effective_timeout_secs());
        let res = tokio::time::timeout(timeout, stdout.read_line(&mut line)).await;
        // 无论成败都回收子进程：读到首行即杀，避免命令继续产出撑爆管道或悬挂。
        let _ = child.kill().await;
        match res {
            Ok(Ok(n)) if n > 0 => Some(line),
            _ => None,
        }
    }
}

/// stdout 首行决策 JSON：`{"decision":"deny","reason":"..."}` / `{"decision":"allow"}`。
#[derive(Deserialize)]
struct HookDecision {
    decision: String,
    reason: Option<String>,
}

/// 解析决策行：`deny` → 拦截原因；`allow` / 未知 decision / 非法 JSON → 放行（`None`）。
fn parse_decision(line: &str) -> Option<String> {
    match serde_json::from_str::<HookDecision>(line) {
        Ok(HookDecision { decision, reason }) if decision == "deny" => {
            Some(reason.unwrap_or_else(|| "被 shell 钩子拦截".into()))
        }
        Ok(d) if d.decision == "allow" => None,
        Ok(d) => {
            tracing::warn!(decision = %d.decision, "shell hook 返回未知 decision，按放行处理");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, line = %line, "shell hook 输出不是合法决策 JSON，按放行处理");
            None
        }
    }
}

#[async_trait::async_trait]
impl Hook for ShellHook {
    /// 观察事件（H20）：**按事件名匹配**规则——`tool_call` 经
    /// [`Hook::before_tool_intercept`] 通道处理（此处跳过以免同一命令触发两次）；
    /// 其余事件（生命周期 / 会话级结构化事件）走通知通道，stdin 收到事件负载 JSON。
    async fn on_event(&self, event: &HookEvent) {
        let expected = self.rule.event.as_str();
        if event.name() != expected {
            return;
        }
        // 工具前事件由拦截通道处理（那里能返回决定），避免重复触发同一命令。
        if matches!(event, HookEvent::BeforeTool { .. }) {
            return;
        }
        // 工具过滤只对工具类事件有意义（其余事件无 `tool` 字段）。
        if let HookEvent::AfterTool { tool, .. } = event {
            if !self.rule.matches_tool(tool) {
                return;
            }
        }
        self.run_first_line(&event.to_payload().to_string()).await;
    }

    /// 工具执行前拦截：`deny` 决策 → `Some(reason)` 阻止执行；其余一律放行。
    async fn before_tool_intercept(&self, tool: &str, args: &serde_json::Value) -> Option<String> {
        if self.rule.event != HookEventKind::ToolCall || !self.rule.matches_tool(tool) {
            return None;
        }
        let payload = json!({ "event": "tool_call", "tool": tool, "args": args });
        self.run_first_line(&payload.to_string())
            .await
            .and_then(|line| parse_decision(&line))
    }
}

/// 由配置装配 shell 钩子列表：`[[hooks]]` 每条规则对应一个 [`ShellHook`]；
/// 空配置返回空 vec。装配层（main）把返回值并入 agent 的 hooks 注入即可。
#[must_use]
pub fn shell_hooks_from_config(cfg: &agent_config::Config) -> Vec<Arc<dyn Hook>> {
    cfg.hooks
        .iter()
        .map(|rule| Arc::new(ShellHook::new(rule.clone())) as Arc<dyn Hook>)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试规则（无工具过滤、显式超时）。
    fn rule(event: HookEventKind, command: &str, timeout_secs: Option<u64>) -> HookRule {
        HookRule {
            event,
            tool: None,
            command: command.into(),
            timeout_secs,
        }
    }

    // deny 决策 → 拦截并带回 reason（真实进程 echo）。
    #[cfg(unix)]
    #[tokio::test]
    async fn deny_decision_blocks() {
        let h = ShellHook::new(rule(
            HookEventKind::ToolCall,
            "echo '{\"decision\":\"deny\",\"reason\":\"不许跑\"}'",
            Some(5),
        ));
        let reason = h
            .before_tool_intercept("shell", &serde_json::json!({ "cmd": "ls" }))
            .await;
        assert_eq!(reason.as_deref(), Some("不许跑"));
    }

    // allow / 非 JSON / 空对象 → 放行。
    #[cfg(unix)]
    #[tokio::test]
    async fn allow_and_garbage_pass_through() {
        for cmd in [
            "echo '{\"decision\":\"allow\"}'",
            "echo 'not-json'",
            "echo '{}'",
            "true", // 无输出（EOF）
        ] {
            let h = ShellHook::new(rule(HookEventKind::ToolCall, cmd, Some(5)));
            let reason = h
                .before_tool_intercept("shell", &serde_json::json!({}))
                .await;
            assert!(reason.is_none(), "应放行: {cmd}");
        }
    }

    /// H20：命名事件按名匹配触发，且 stdin 负载带 `event` 字段（真实 shell 校验）。
    #[cfg(unix)]
    #[tokio::test]
    async fn named_event_matches_by_name_and_writes_payload() {
        // 命令把 stdin 原样写到文件，供断言负载形状。
        let out = std::env::temp_dir().join(format!("gyre-hook-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let hook = ShellHook::new(rule(
            HookEventKind::TurnStart,
            &format!("cat > {}", out.display()),
            Some(5),
        ));
        // 名字不匹配 → 不触发。
        hook.on_event(&HookEvent::named("agent_start", serde_json::json!({})))
            .await;
        assert!(!out.exists(), "不匹配的事件不应触发命令");
        // 名字匹配 → 触发并写入负载。
        hook.on_event(&HookEvent::named(
            "turn_start",
            serde_json::json!({"turn": 2}),
        ))
        .await;
        let text = std::fs::read_to_string(&out).expect("命令应写入 stdin 负载");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["event"], "turn_start");
        assert_eq!(v["turn"], 2);
        let _ = std::fs::remove_file(&out);
    }

    /// H20：`tool_call` 只走拦截通道（`on_event` 不重复触发命令）。
    #[cfg(unix)]
    #[tokio::test]
    async fn tool_call_event_does_not_double_fire() {
        let out = std::env::temp_dir().join(format!("gyre-hook-tc-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let hook = ShellHook::new(rule(
            HookEventKind::ToolCall,
            &format!("echo hit >> {}", out.display()),
            Some(5),
        ));
        // on_event（观察通道）不得触发；否则「拦截 + 通知」会各跑一次。
        hook.on_event(&HookEvent::BeforeTool {
            tool: "shell".into(),
            args: serde_json::json!({}),
        })
        .await;
        assert!(!out.exists(), "tool_call 不应经 on_event 触发");
        let _ = std::fs::remove_file(&out);
    }
    // 命令不存在（shell 内部 127）→ 放行。
    #[cfg(unix)]
    #[tokio::test]
    async fn command_failure_passes_through() {
        let h = ShellHook::new(rule(
            HookEventKind::ToolCall,
            "/nonexistent-bin-xyz",
            Some(5),
        ));
        assert!(
            h.before_tool_intercept("shell", &serde_json::json!({}))
                .await
                .is_none()
        );
    }

    // 超时 → 放行，且在超时点及时返回（子进程被 kill，不悬挂到 sleep 结束）。
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_passes_through_and_kills() {
        let h = ShellHook::new(rule(HookEventKind::ToolCall, "sleep 30", Some(1)));
        let t = std::time::Instant::now();
        let reason = h
            .before_tool_intercept("shell", &serde_json::json!({}))
            .await;
        assert!(reason.is_none(), "超时应放行");
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "应在超时后及时返回，实际 {:?}",
            t.elapsed()
        );
    }

    // 工具名过滤：不命中时不触发命令，命中时拦截。
    #[cfg(unix)]
    #[tokio::test]
    async fn tool_filter_gates_trigger() {
        let mut r = rule(
            HookEventKind::ToolCall,
            "echo '{\"decision\":\"deny\",\"reason\":\"x\"}'",
            Some(5),
        );
        r.tool = Some("shell".into());
        let h = ShellHook::new(r);
        assert!(
            h.before_tool_intercept("read", &serde_json::json!({}))
                .await
                .is_none(),
            "非目标工具应放行"
        );
        assert_eq!(
            h.before_tool_intercept("shell", &serde_json::json!({}))
                .await
                .as_deref(),
            Some("x")
        );
    }

    // after_tool 通知型：命令被触发（副作用写文件为证）。
    #[cfg(unix)]
    #[tokio::test]
    async fn after_tool_notification_fires() {
        let path = std::env::temp_dir().join(format!("gyre-hook-after-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cmd = format!("echo fired >> {}", path.display());
        let h = ShellHook::new(rule(HookEventKind::ToolResult, &cmd, Some(5)));
        h.on_event(&HookEvent::AfterTool {
            tool: "shell".into(),
            result: agent_core::ToolResult::text("ok"),
        })
        .await;
        assert!(path.exists(), "after_tool 通知应触发命令");
        let _ = std::fs::remove_file(&path);
    }

    // stop 通知型：命令被触发。
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_notification_fires() {
        let path = std::env::temp_dir().join(format!("gyre-hook-stop-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cmd = format!("echo fired >> {}", path.display());
        let h = ShellHook::new(rule(HookEventKind::Stop, &cmd, Some(5)));
        h.on_event(&HookEvent::Stop { success: true }).await;
        assert!(path.exists(), "stop 通知应触发命令");
        let _ = std::fs::remove_file(&path);
    }

    // before_tool 规则在 on_event 里不触发（避免与 intercept 通道双发）。
    #[cfg(unix)]
    #[tokio::test]
    async fn before_tool_rule_skips_on_event() {
        let path = std::env::temp_dir().join(format!("gyre-hook-skip-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cmd = format!("echo fired >> {}", path.display());
        let h = ShellHook::new(rule(HookEventKind::ToolCall, &cmd, Some(5)));
        h.on_event(&HookEvent::BeforeTool {
            tool: "shell".into(),
            args: serde_json::json!({}),
        })
        .await;
        assert!(
            !path.exists(),
            "before_tool 应走 intercept 通道，on_event 不触发"
        );
    }

    // shell_hooks_from_config：规则一一映射；空配置返回空 vec。
    #[test]
    fn from_config_maps_rules() {
        let src = r#"
[default_model]
id = "m"
api = "deepseek"
base_url = "x"

[[hooks]]
event = "before_tool"
command = "/bin/gate"

[[hooks]]
event = "after_tool"
command = "/bin/notify"
"#;
        let cfg: agent_config::Config = toml::from_str(src).unwrap();
        assert_eq!(shell_hooks_from_config(&cfg).len(), 2);

        let empty: agent_config::Config =
            toml::from_str("[default_model]\nid = \"m\"\napi = \"deepseek\"\nbase_url = \"x\"\n")
                .unwrap();
        assert!(
            shell_hooks_from_config(&empty).is_empty(),
            "空配置返回空 vec"
        );
    }
}
