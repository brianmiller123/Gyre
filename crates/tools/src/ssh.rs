//! SSH 工具：解析 `~/.ssh/config` 并在远端主机执行命令。
//!
//! 移植方向：oh-my-pi 的 ssh 能力（`coding-agent/src/ssh/` 与相关 CLI）。v1 定位为
//! **无状态直连**：不维护长连接（`list`/`disconnect` 为占位语义）、不做 sshfs 挂载
//! （P2 后续）、不支持交互式会话与密码认证（`BatchMode=yes` + stdin 关闭，避免挂起）。
//!
//! `ssh_config` 解析为轻量 OpenSSH 子集：仅识别 `Host`/`HostName`/`Port`/`User`/
//! `IdentityFile`/`ProxyJump`，支持 `Host *` 通配与精确匹配（**精确优先**——与 OpenSSH
//! 「文件顺序先得」不同，这是对用户直觉的刻意简化，避免 `Host *` 默认段覆盖精确别名）、
//! `!` 否定、引号值与行内注释；不支持 `Include`/`Match`/续行（v1 静默忽略）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use agent_core::{ApprovalRequest, CapabilityTier, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::json;

use crate::{Tool, ToolContext};

/// 注入 system prompt 的 SSH 工具使用指引（启用时由装配层追加到上下文）。
pub const SSH_PROMPT_SECTION: &str = "<ssh>\n\
SSH 工具 `ssh` 已启用：解析 ~/.ssh/config（Host/HostName/Port/User/IdentityFile/ProxyJump），\
在远端主机执行命令。\n\
- connect <host>：解析并校验主机配置，返回解析后的连接参数（不建立真实连接）。\n\
- exec <host> <command>：在远端执行命令（非交互 BatchMode，120s 超时，stdout/stderr 合并返回，超 200KiB 截断）。\n\
- list：列出 ~/.ssh/config 中可用主机（v1 不维护长连接，无活动会话）。\n\
- disconnect <host>：no-op（v1 每次 exec 直连）。\n\
v1 限制：不支持 sshfs 挂载（P2 后续）、交互式会话与密码认证；优先使用配置了\n\
IdentityFile/ProxyJump 的主机。\n\
</ssh>";

/// SSH 工具：解析 `~/.ssh/config` 并直连执行远端命令（v1 无状态）。
///
/// 动作：
/// - `connect`：解析并校验主机配置（Host/Port/User/IdentityFile/ProxyJump），返回解析后的
///   连接参数，**不**建立真实连接。
/// - `exec`：`ssh <host> '<command>'` 直连执行，120s 墙钟超时，stdout/stderr 合并、
///   按 200KiB 截断返回。非交互（`BatchMode=yes` + stdin 关闭），避免密码提示挂起。
/// - `list`：列出 `~/.ssh/config` 可用主机；v1 不维护长连接，故无「已连接会话」。
/// - `disconnect`：no-op，返回 `ok`。
#[derive(Debug)]
pub struct SshTool {
    /// 显式 ssh 配置路径（`None` = 默认 `~/.ssh/config`）。注入路径便于测试与多配置装配。
    config_path: Option<PathBuf>,
}

impl SshTool {
    /// 构造 SSH 工具；`config_path` 为 `None` 时使用默认 `~/.ssh/config`。
    #[must_use]
    pub const fn new(config_path: Option<PathBuf>) -> Self {
        Self { config_path }
    }

    /// 读取并解析 ssh 配置（缺文件/不可读时给出明确错误）。
    fn load_config(&self) -> Result<SshConfig, ToolError> {
        let path = self.config_path.clone().unwrap_or_else(default_config_path);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::Execution(format!(
                    "SSH 配置文件不存在：{}。请先配置 ~/.ssh/config \
                     （Host/HostName/Port/User/IdentityFile/ProxyJump），\
                     或经 SshTool::new 注入显式路径。",
                    path.display()
                )));
            }
            Err(e) => return Err(ToolError::Io(e)),
        };
        Ok(SshConfig::parse(&text))
    }

    /// `connect`：解析并校验主机配置，返回解析后的连接参数文本。
    fn connect(&self, host: &str) -> Result<ToolResult, ToolError> {
        let config = self.load_config()?;
        let resolved = config
            .resolve(host)?
            .ok_or_else(|| not_found_error(host, &config))?;
        Ok(ToolResult::text(format!(
            "主机 `{host}` 配置校验通过（v1 不建立长连接，每次 exec 直连）：\n\
             - HostName: {}\n\
             - Port: {}\n\
             - User: {}\n\
             - IdentityFile: {}\n\
             - ProxyJump: {}\n\
             （sshfs 挂载为 P2 后续）",
            resolved.hostname,
            resolved.port,
            resolved.user.as_deref().unwrap_or("(默认)"),
            resolved.identity_file.as_deref().unwrap_or("(未指定)"),
            resolved.proxy_jump.as_deref().unwrap_or("(未指定)"),
        )))
    }

    /// `exec`：解析主机 → 构造参数 → 直连执行（120s 超时，输出合并截断）。
    async fn exec(&self, host: &str, command: &str) -> Result<ToolResult, ToolError> {
        let config = self.load_config()?;
        let resolved = config
            .resolve(host)?
            .ok_or_else(|| not_found_error(host, &config))?;
        let args = build_ssh_args(&resolved, command);
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // 墙钟超时兜底：v1 不可中断（interruptible=false），超时是唯一退出路径。
        let output = tokio::time::timeout(SSH_TIMEOUT, run_ssh_capped(cmd)).await;
        match output {
            Ok(Ok(ssh_output)) => {
                let text = if ssh_output.combined.is_empty() {
                    "(ssh 执行成功，无输出)".to_string()
                } else {
                    ssh_output.combined
                };
                if ssh_output.status.is_some_and(|s| s.success()) {
                    Ok(ToolResult::text(text))
                } else {
                    Ok(ToolResult::text(format!(
                        "[exit {}]\n{text}",
                        ssh_output.status.and_then(|s| s.code()).unwrap_or(-1)
                    )))
                }
            }
            Ok(Err(e)) => Err(ToolError::Io(e)),
            Err(_) => Err(ToolError::Execution(format!(
                "ssh 命令超时（{SSH_TIMEOUT:?}），主机可能不可达或远端命令阻塞"
            ))),
        }
    }

    /// `list`：列出 `~/.ssh/config` 可用主机；v1 无活动会话。
    fn list(&self) -> Result<ToolResult, ToolError> {
        let config = self.load_config()?;
        let names = config.host_names();
        let body = if names.is_empty() {
            "（配置中未找到任何主机别名）".to_string()
        } else {
            format!("可用主机（{}）：\n- {}", names.len(), names.join("\n- "))
        };
        Ok(ToolResult::text(format!(
            "v1 不维护长连接（每次 exec 直连），当前无活动会话。\n{body}"
        )))
    }

    /// `disconnect`：v1 为 no-op（无长连接可断开），返回 `ok`。
    fn disconnect(&self, _host: Option<&str>) -> ToolResult {
        ToolResult::text("ok（v1 不维护长连接，无会话可断开）")
    }
}

impl Default for SshTool {
    fn default() -> Self {
        Self::new(None)
    }
}

#[async_trait]
impl Tool for SshTool {
    fn name(&self) -> &'static str {
        "ssh"
    }

    fn description(&self) -> &'static str {
        "解析 ~/.ssh/config 并在远端主机执行命令（action ∈ connect/exec/list/disconnect）。\
         属于执行类操作，默认需审批；v1 每次 exec 直连，不维护长连接。"
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["connect", "exec", "list", "disconnect"],
                    "description": "操作：connect=解析并校验主机配置；exec=远端执行命令；\
                                     list=列出可用主机；disconnect=断开（v1 no-op）"
                },
                "host": {
                    "type": "string",
                    "description": "~/.ssh/config 中的主机别名"
                },
                "command": {
                    "type": "string",
                    "description": "exec 时在远端执行的命令（原样传给远端 shell）"
                }
            },
            "required": ["action"]
        })
    }

    fn capability(&self) -> CapabilityTier {
        CapabilityTier::Execute
    }

    /// 不可中断：v1 每次 exec 有 120s 墙钟超时兜底，且 `BatchMode=yes` 下不会出现密码交互
    /// 挂起，无需 steering 批级取消（与 shell 工具不同，后者可被中途打断）。
    fn interruptible(&self) -> bool {
        false
    }

    fn describe<'a>(&'a self, input: &'a serde_json::Value) -> ApprovalRequest<'a> {
        ApprovalRequest {
            tool: self.name(),
            capability: self.capability(),
            command: input.get("host").and_then(serde_json::Value::as_str),
            args: input,
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let action = input
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("缺少 `action` 参数".into()))?;
        match action {
            "connect" => {
                let host = required_str(&input, "host")?;
                self.connect(host)
            }
            "exec" => {
                let host = required_str(&input, "host")?;
                let command = required_str(&input, "command")?;
                self.exec(host, command).await
            }
            "list" => self.list(),
            "disconnect" => {
                Ok(self.disconnect(input.get("host").and_then(serde_json::Value::as_str)))
            }
            other => Err(ToolError::InvalidArgs(format!(
                "未知 action `{other}`（可用：connect/exec/list/disconnect）"
            ))),
        }
    }
}

/// 从工具入参取必填字符串参数。
fn required_str<'a>(input: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolError> {
    input
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolError::InvalidArgs(format!("缺少 `{key}` 参数")))
}

// ── ssh_config 解析（轻量 OpenSSH 子集）────────────────────────────────────────

/// 一个 `Host <patterns>` 段（到下一个 `Host` 关键字为止）。
#[derive(Debug)]
struct SshHostSection {
    /// Host 后的模式列表（可含 `!` 否定前缀与 `*`/`?` 通配）。
    patterns: Vec<String>,
    /// 关键字（小写）→ 值（每段内首次出现生效）。
    kv: HashMap<&'static str, String>,
}

/// 解析后的 `~/.ssh/config`。
#[derive(Debug)]
struct SshConfig {
    sections: Vec<SshHostSection>,
}

/// 段匹配结果（精确 > 通配 > 不匹配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchKind {
    /// 含字面别名精确命中。
    Exact,
    /// 仅经 `*`/`?` 通配命中。
    Wildcard,
    /// 不匹配（或被 `!` 否定排除）。
    None,
}

impl SshConfig {
    /// 解析 `ssh_config` 文本（忽略注释/空行/未知关键字；不支持 `Include`/`Match`/续行）。
    fn parse(text: &str) -> Self {
        let mut sections: Vec<SshHostSection> = Vec::new();
        for raw in text.lines() {
            let line = strip_comment(raw).trim().to_string();
            if line.is_empty() {
                continue;
            }
            let mut tokens = split_tokens(&line).into_iter();
            let Some(keyword) = tokens.next() else {
                continue;
            };
            let keyword = keyword.to_ascii_lowercase();
            if keyword == "host" {
                let patterns: Vec<String> = tokens
                    .flat_map(|t| t.split(',').map(ToOwned::to_owned).collect::<Vec<_>>())
                    .map(|t| unquote(&t))
                    .filter(|p| !p.is_empty())
                    .collect();
                sections.push(SshHostSection {
                    patterns,
                    kv: HashMap::new(),
                });
                continue;
            }
            if let Some(key) = normalize_keyword(&keyword) {
                if let Some(section) = sections.last_mut() {
                    // 值取关键字后的全部 token 以空格连接（恢复引号内的空白），再去引号。
                    let value = unquote(&tokens.collect::<Vec<_>>().join(" "));
                    section.kv.entry(key).or_insert(value);
                }
                // Host 段之前的零散设置：v1 忽略。
            }
            // 其余关键字（Include/Match/StrictHostKeyChecking 等）：v1 忽略。
        }
        Self { sections }
    }

    /// 为指定主机解析连接参数（精确匹配优先于通配；同级别内文件顺序先得）。
    ///
    /// # Errors
    /// 命中段的 `Port` 配置非法（非 1..=65535）时返回 [`ToolError::Execution`]。
    fn resolve(&self, host: &str) -> Result<Option<ResolvedHost>, ToolError> {
        let mut exact: Vec<&SshHostSection> = Vec::new();
        let mut wild: Vec<&SshHostSection> = Vec::new();
        for section in &self.sections {
            match section.match_kind(host) {
                MatchKind::Exact => exact.push(section),
                MatchKind::Wildcard => wild.push(section),
                MatchKind::None => {}
            }
        }
        if exact.is_empty() && wild.is_empty() {
            return Ok(None);
        }
        let mut kv: HashMap<&'static str, String> = HashMap::new();
        for section in exact.into_iter().chain(wild) {
            for (key, value) in &section.kv {
                kv.entry(*key).or_insert(value.clone());
            }
        }
        let port = match kv.get("port") {
            Some(raw) => raw.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| {
                ToolError::Execution(format!("主机 `{host}` 的 Port 配置无效：{raw}"))
            })?,
            None => 22,
        };
        Ok(Some(ResolvedHost {
            host: host.to_string(),
            hostname: kv
                .get("hostname")
                .cloned()
                .unwrap_or_else(|| host.to_string()),
            port,
            user: kv.get("user").cloned(),
            identity_file: kv.get("identityfile").map(|v| expand_tilde(v)),
            proxy_jump: kv.get("proxyjump").map(|v| expand_tilde(v)),
        }))
    }

    /// 收集配置中所有非通配、非否定的主机别名（去重、排序），用于 list 与错误提示。
    fn host_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for section in &self.sections {
            for raw in &section.patterns {
                if raw.starts_with('!') {
                    continue;
                }
                if !raw.is_empty() && !raw.contains('*') && !raw.contains('?') {
                    names.push(raw.clone());
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }
}

impl SshHostSection {
    /// 判断本段是否匹配给定主机别名（大小写不敏感，`!` 否定优先排除）。
    fn match_kind(&self, host: &str) -> MatchKind {
        let mut exact = false;
        let mut wild = false;
        let mut excluded = false;
        for raw in &self.patterns {
            let (negated, pattern) = match raw.strip_prefix('!') {
                Some(p) => (true, p),
                None => (false, raw.as_str()),
            };
            if pattern.is_empty() || !host_pattern_matches(pattern, host) {
                continue;
            }
            if negated {
                excluded = true;
            } else if pattern.contains('*') || pattern.contains('?') {
                wild = true;
            } else {
                exact = true;
            }
        }
        if excluded {
            MatchKind::None
        } else if exact {
            MatchKind::Exact
        } else if wild {
            MatchKind::Wildcard
        } else {
            MatchKind::None
        }
    }
}

/// 为指定主机解析出的连接参数。
#[derive(Debug)]
struct ResolvedHost {
    /// 请求的主机别名（作为 ssh 目标）。
    host: String,
    /// 实际连接地址（未配置 `HostName` 时为别名本身）。
    hostname: String,
    /// 端口（默认 22）。
    port: u16,
    /// 用户名（未配置时为 None，沿用本地用户）。
    user: Option<String>,
    /// `IdentityFile` 路径（已展开 `~`；未配置为 None）。
    identity_file: Option<String>,
    /// `ProxyJump` 目标（已展开 `~`；未配置为 None）。
    proxy_jump: Option<String>,
}

/// 主机未找到时的明确错误：列出可用主机别名。
fn not_found_error(host: &str, config: &SshConfig) -> ToolError {
    let names = config.host_names();
    let hint = if names.is_empty() {
        "配置中未找到任何主机别名".to_string()
    } else {
        format!("可用主机别名：{}", names.join(", "))
    };
    ToolError::Execution(format!(
        "无法解析 SSH 主机 `{host}`：配置中没有匹配的 Host 段（支持精确匹配与 `Host *` 通配）。\n{hint}"
    ))
}

/// 把关键字归一化为小写键名；仅识别本工具需要的子集，其余返回 `None`。
fn normalize_keyword(keyword: &str) -> Option<&'static str> {
    match keyword {
        "hostname" => Some("hostname"),
        "port" => Some("port"),
        "user" => Some("user"),
        "identityfile" => Some("identityfile"),
        "proxyjump" => Some("proxyjump"),
        _ => None,
    }
}

/// 主机模式匹配：`*` 匹配任意序列、`?` 匹配单个字符，大小写不敏感。
fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    let mut regex_text = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => regex_text.push_str(".*"),
            '?' => regex_text.push('.'),
            c => regex_text.push_str(&regex::escape(&c.to_string())),
        }
    }
    regex_text.push('$');
    regex::RegexBuilder::new(&regex_text)
        .case_insensitive(true)
        .build()
        .is_ok_and(|re| re.is_match(host))
}

/// 去掉行内注释（`#` 到行尾；引号内的 `#` 保留）。
fn strip_comment(line: &str) -> &str {
    let mut in_quote = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_quote = !in_quote,
            '#' if !in_quote => return &line[..i],
            _ => {}
        }
    }
    line
}

/// 按空白切分 token；引号内的空白不切分（支持 `HostName "my host"` 等带空格值）。
fn split_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                cur.push(ch);
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// 去掉值两侧的成对双引号（`"value"` → `value`）；非成对引号原样保留。
fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// 用户主目录（Unix 取 `$HOME`，Windows 取 `%USERPROFILE%`）。
fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// 默认 ssh 配置路径：`~/.ssh/config`（Unix）/ `%USERPROFILE%\.ssh\config`（Windows）。
fn default_config_path() -> PathBuf {
    home_dir().map_or_else(
        || PathBuf::from(".ssh/config"),
        |home| home.join(".ssh").join("config"),
    )
}

/// 展开值开头的 `~` 为家目录（`~/x` → 家目录/x，`~` → 家目录）；无家目录或值不以 `~`
/// 开头时原样返回（相对路径交给 ssh 按 `~/.ssh` 解析）。`~user/...` 形式不支持，原样保留。
fn expand_tilde(value: &str) -> String {
    let Some(rest) = value.strip_prefix('~') else {
        return value.to_string();
    };
    let Some(home) = home_dir() else {
        return value.to_string();
    };
    if rest.is_empty() {
        home.to_string_lossy().into_owned()
    } else if let Some(trimmed) = rest.strip_prefix('/') {
        home.join(trimmed).to_string_lossy().into_owned()
    } else {
        value.to_string()
    }
}

// ── exec 参数构造与执行 ────────────────────────────────────────────────────────

/// 命令墙钟超时上限。
const SSH_TIMEOUT: Duration = Duration::from_secs(120);
/// 合并输出大小上限（超出截断，防爆上下文）。
const SSH_MAX_OUTPUT: usize = 200 * 1024;
/// 连接超时（秒）：不可达主机快速失败，不占满 120s。
const SSH_CONNECT_TIMEOUT_SECS: u64 = 15;

/// 构造 `ssh` 命令参数（无 shell 包装，逐参数传入，避免本地 shell 解释）。
///
/// 解析结果全部以显式参数钉死（`-p`/`-l`/`-i`/`-J`/`-o HostName=`），保证「精确优先」
/// 的解析语义不被 ssh 自身的配置读取（文件顺序先得）覆盖。
fn build_ssh_args(host: &ResolvedHost, command: &str) -> Vec<String> {
    let mut args = vec![
        // 非交互：-T 不分配伪终端；BatchMode 禁止密码提示（stdin 已关闭，避免挂起）。
        "-T".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"),
    ];
    if host.port != 22 {
        args.push("-p".to_string());
        args.push(host.port.to_string());
    }
    if let Some(user) = &host.user {
        args.push("-l".to_string());
        args.push(user.clone());
    }
    if let Some(identity) = &host.identity_file {
        args.push("-i".to_string());
        args.push(identity.clone());
    }
    if let Some(jump) = &host.proxy_jump {
        args.push("-J".to_string());
        args.push(jump.clone());
    }
    if host.hostname != host.host {
        args.push("-o".to_string());
        args.push(format!("HostName={}", host.hostname));
    }
    // 目标别名 + 命令（命令作为单个参数，经 ssh 原样拼给远端 shell）。
    args.push(host.host.clone());
    args.push(command.to_string());
    args
}

/// 子进程执行结果（流式有界读取后）。
struct SshOutput {
    /// 退出状态。
    status: Option<std::process::ExitStatus>,
    /// 合并后的输出文本（已截断）。
    combined: String,
}

/// 启动 `ssh` 子进程并并发读取 stdout/stderr（各按 `SSH_MAX_OUTPUT` 上限，防爆内存），
/// 等待退出后合并为单段文本。
async fn run_ssh_capped(mut cmd: tokio::process::Command) -> std::io::Result<SshOutput> {
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // 并发读取 stdout/stderr 与等待子进程退出——使用 join 而非 spawn，
    // 确保 future 被 drop（超时）时读取任务也随之取消，不留孤儿。
    let (status, out, err) = tokio::join!(
        child.wait(),
        read_capped(stdout, SSH_MAX_OUTPUT),
        read_capped(stderr, SSH_MAX_OUTPUT),
    );
    Ok(SshOutput {
        status: Some(status?),
        combined: combine_capped(out, err),
    })
}

/// 读取流的前 `max` 字节保留；触及上限后继续**丢弃式读取**直到 EOF，
/// 保证管道持续排空、子进程不阻塞（仅返回给上层的文本被截断到 `max`）。
async fn read_capped<R>(reader: Option<R>, max: usize) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let Some(mut reader) = reader else {
        return Vec::new();
    };
    let mut buf = Vec::with_capacity(max.min(64 * 1024));
    let mut tmp = [0u8; 8192];
    let mut capped = false;
    loop {
        match reader.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                if !capped {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.len() >= max {
                        buf.truncate(max);
                        capped = true;
                    }
                }
                // capped 后继续读但不保存：保持管道排空，子进程不阻塞。
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(target: "tools::ssh", "流读取错误（返回已读部分）: {e}");
                break;
            }
        }
    }
    buf
}

/// 合并 stdout/stderr 为单段文本，并按总上限截断（UTF-8 边界安全）。
fn combine_capped(out: Vec<u8>, err: Vec<u8>) -> String {
    let mut combined = String::new();
    if !out.is_empty() {
        combined.push_str(&String::from_utf8_lossy(&out));
    }
    if !err.is_empty() {
        if !combined.is_empty() {
            combined.push_str("\n[stderr]\n");
        }
        combined.push_str(&String::from_utf8_lossy(&err));
    }
    truncate_text(combined, SSH_MAX_OUTPUT)
}

/// 截断文本到 `max` 字节：回退到最近 UTF-8 字符边界，并追加截断标记。
fn truncate_text(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str("\n...(输出过长，已截断)");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 截断标记（与生产常量保持一致，避免测试魔法字符串漂移）。
    const TRUNCATE_MARKER: &str = "\n...(输出过长，已截断)";

    /// 测试用解析入口（与生产共用同一 `SshConfig::parse` 路径）。
    fn parse(text: &str) -> SshConfig {
        SshConfig::parse(text)
    }

    /// 构造「仅别名、全默认」的 ResolvedHost，供参数构造测试按需覆写字段。
    fn resolved(host: &str) -> ResolvedHost {
        ResolvedHost {
            host: host.to_string(),
            hostname: host.to_string(),
            port: 22,
            user: None,
            identity_file: None,
            proxy_jump: None,
        }
    }

    const SAMPLE: &str = "\
# 默认段（通配）
Host *
    HostName default-gw.example.com
    User ubuntu
    Port 2222

# 注释段
Host web-prod
    HostName 10.0.0.5
    User deploy
    IdentityFile ~/.ssh/deploy_key  # 行内注释
    ProxyJump bastion

Host \"quoted host\"
    Port 2200
";

    #[test]
    fn parse_config_multiple_sections() {
        let cfg = parse(SAMPLE);
        assert_eq!(cfg.sections.len(), 3);
        let web = cfg
            .sections
            .iter()
            .find(|s| s.patterns == ["web-prod"])
            .expect("应有 web-prod 段");
        assert_eq!(web.kv.get("hostname").map(String::as_str), Some("10.0.0.5"));
        assert_eq!(web.kv.get("user").map(String::as_str), Some("deploy"));
        assert_eq!(
            web.kv.get("identityfile").map(String::as_str),
            Some("~/.ssh/deploy_key")
        );
        assert_eq!(web.kv.get("proxyjump").map(String::as_str), Some("bastion"));
        // 段之间互不污染：web-prod 段本身没有 Port。
        assert_eq!(web.kv.get("port"), None);
    }

    #[test]
    fn parse_config_quoted_values() {
        let cfg =
            parse("Host box\n    HostName \"my box\"\n    IdentityFile \"/path with space/id\"\n");
        assert_eq!(cfg.sections.len(), 1);
        let section = &cfg.sections[0];
        assert_eq!(
            section.kv.get("hostname").map(String::as_str),
            Some("my box")
        );
        assert_eq!(
            section.kv.get("identityfile").map(String::as_str),
            Some("/path with space/id")
        );
    }

    #[test]
    fn parse_config_quoted_host_pattern() {
        // 带空格的主机别名应整体保留为一个 pattern。
        let cfg = parse(SAMPLE);
        assert_eq!(cfg.sections[2].patterns, ["quoted host"]);
    }

    #[test]
    fn parse_config_comments_and_blanks_ignored() {
        let cfg = parse(
            "\n  \n# 整行注释\nHost a\n  # 缩进注释\n  User u1  # 行内注释\n\nHost b\n\tUser u2\n",
        );
        assert_eq!(cfg.sections.len(), 2);
        assert_eq!(
            cfg.sections[0].kv.get("user").map(String::as_str),
            Some("u1")
        );
        assert_eq!(
            cfg.sections[1].kv.get("user").map(String::as_str),
            Some("u2")
        );
    }

    #[test]
    fn parse_config_keywords_case_insensitive() {
        let cfg = parse("Host c\n    HOSTNAME example.com\n    PORT 2200\n    USER alice\n");
        let section = &cfg.sections[0];
        assert_eq!(
            section.kv.get("hostname").map(String::as_str),
            Some("example.com")
        );
        assert_eq!(section.kv.get("port").map(String::as_str), Some("2200"));
        assert_eq!(section.kv.get("user").map(String::as_str), Some("alice"));
    }

    #[test]
    fn resolve_exact_host_beats_wildcard_default() {
        let cfg = parse("Host *\n    User default-user\n\nHost myserver\n    User alice\n");
        let r = cfg
            .resolve("myserver")
            .expect("解析不失败")
            .expect("应命中");
        assert_eq!(r.user.as_deref(), Some("alice"));
        assert_eq!(r.port, 22);
        assert_eq!(r.hostname, "myserver");
    }

    #[test]
    fn resolve_wildcard_fallback_with_defaults() {
        let cfg = parse("Host *\n    User ubuntu\n    Port 2222\n");
        let r = cfg.resolve("anyhost").expect("解析不失败").expect("应命中");
        assert_eq!(r.hostname, "anyhost"); // 未配置 HostName 时回落为别名
        assert_eq!(r.user.as_deref(), Some("ubuntu"));
        assert_eq!(r.port, 2222);
    }

    #[test]
    fn resolve_missing_host_returns_none_and_lists_aliases() {
        let cfg = parse("Host web-prod\n    HostName 10.0.0.5\nHost db-1\n    Port 5433\n");
        assert!(cfg.resolve("nope").expect("解析不失败").is_none());
        let msg = not_found_error("nope", &cfg).to_string();
        assert!(msg.contains("nope"), "错误应提及目标主机: {msg}");
        assert!(msg.contains("web-prod"), "错误应列出可用主机: {msg}");
        assert!(msg.contains("db-1"), "错误应列出可用主机: {msg}");
    }

    #[test]
    fn resolve_negation_pattern_excludes_host() {
        let cfg = parse("Host * !banned\n    User ok-user\n");
        assert!(cfg.resolve("banned").expect("解析不失败").is_none());
        let r = cfg.resolve("allowed").expect("解析不失败").expect("应命中");
        assert_eq!(r.user.as_deref(), Some("ok-user"));
    }

    #[test]
    fn resolve_invalid_port_errors() {
        for port in ["0", "abc", "70000"] {
            let cfg = parse(&format!("Host broken\n    Port {port}\n"));
            let err = cfg.resolve("broken").expect_err("无效 Port 应报错");
            assert!(err.to_string().contains("broken"), "错误应提及主机: {err}");
        }
    }

    #[test]
    fn build_ssh_args_includes_port_identity_proxy() {
        let mut h = resolved("web-prod");
        h.hostname = "10.0.0.5".into();
        h.port = 2222;
        h.user = Some("deploy".into());
        h.identity_file = Some("/home/u/.ssh/deploy_key".into());
        h.proxy_jump = Some("bastion".into());
        let args = build_ssh_args(&h, "uname -a");
        assert_eq!(
            args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "-p",
                "2222",
                "-l",
                "deploy",
                "-i",
                "/home/u/.ssh/deploy_key",
                "-J",
                "bastion",
                "-o",
                "HostName=10.0.0.5",
                "web-prod",
                "uname -a",
            ]
        );
    }

    #[test]
    fn build_ssh_args_minimal_defaults() {
        let h = resolved("defaults");
        let args = build_ssh_args(&h, "pwd");
        assert_eq!(
            args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "defaults",
                "pwd"
            ]
        );
    }

    #[test]
    fn truncate_text_long_output_caps_at_boundary() {
        // 中文字符串：截断点必须回退到 UTF-8 字符边界，且整体仍为合法 UTF-8。
        let truncated = truncate_text("中".repeat(1000), 100);
        assert!(truncated.ends_with(TRUNCATE_MARKER));
        assert!(truncated.len() - TRUNCATE_MARKER.len() <= 100);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_text_short_output_untouched() {
        assert_eq!(truncate_text("hello".to_string(), 100), "hello");
    }

    #[test]
    fn combine_capped_merges_stdout_and_stderr() {
        let combined = combine_capped(b"out-line".to_vec(), b"err-line".to_vec());
        assert!(combined.contains("out-line"));
        assert!(combined.contains("[stderr]"));
        assert!(combined.contains("err-line"));
        assert!(combined.starts_with("out-line"));
    }

    #[test]
    fn host_names_lists_only_literal_aliases() {
        let cfg = parse("Host *\n    User u\nHost b a\nHost !excluded\n    User x\n");
        assert_eq!(cfg.host_names(), ["a", "b"]);
    }

    #[test]
    fn default_config_path_is_dot_ssh_config() {
        let path = default_config_path();
        assert_eq!(path.file_name().and_then(|s| s.to_str()), Some("config"));
        assert!(path.to_string_lossy().contains(".ssh"));
    }

    #[test]
    fn expand_tilde_replaces_home_prefix() {
        let Some(home) = home_dir() else {
            return; // 无家目录环境（罕见）：跳过
        };
        assert_eq!(
            expand_tilde("~/keys/id"),
            home.join("keys/id").to_string_lossy().into_owned()
        );
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        // `~user/...` 形式本工具不支持，原样保留。
        assert_eq!(expand_tilde("~user/keys"), "~user/keys");
    }

    #[test]
    fn connect_resolves_and_reports_parameters() {
        let dir = std::env::temp_dir().join(format!("ssh-tool-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config");
        std::fs::write(
            &path,
            "Host web-prod\n    HostName 10.0.0.5\n    Port 2200\n    User deploy\n",
        )
        .unwrap();
        let tool = SshTool::new(Some(path));
        let result = tool.connect("web-prod").expect("connect 应成功");
        let text = result.to_llm_text();
        assert!(text.contains("web-prod"));
        assert!(text.contains("10.0.0.5"));
        assert!(text.contains("2200"));
        assert!(text.contains("deploy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实 ssh 连接冒烟测试：解析 `~/.ssh/config` 第一个可用主机并 `exec`。
    /// 默认忽略——需要本机 ssh 与可达主机；`cargo test -- --ignored` 手动运行。
    #[tokio::test]
    #[ignore = "需要本机 ssh 与可达主机，CI 默认不跑"]
    async fn exec_real_connection_smoke() {
        let tool = SshTool::default();
        let Ok(config) = tool.load_config() else {
            return; // 无 ~/.ssh/config：跳过
        };
        let Some(host) = config.host_names().first().cloned() else {
            return; // 无可用主机：跳过
        };
        let result = tool.exec(&host, "echo ssh-ok").await.expect("exec 应成功");
        let text = result.to_llm_text();
        assert!(
            text.contains("ssh-ok") || text.contains("[exit"),
            "输出异常: {text}"
        );
    }
}
