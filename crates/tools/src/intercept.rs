//! `run_command` 命令拦截器：把「有专用工具等价物」的 shell 文件 I/O 命令重定向到专用工具。
//!
//! 移植 oh-my-pi `bash-interceptor.ts` 的设计：在子进程 spawn 之前匹配命令，命中即返回
//! 教学性错误（指向 `read_file` / `grep` / `glob` / `write_file`），命令不执行。错误经
//! `ToolError::Execution`（可恢复）回灌给模型，模型据此改用专用工具——既不污染会话，也
//! 不计入连续错误预算（`is_recoverable` 为真）。
//!
//! 与 oh-my-pi 的差异：Rust `regex`（RE2）不支持前瞻/后顾断言，无法直接移植其 echo 重定向
//! 巨型正则。这里把三条简单前缀规则（cat/grep/find）走正则，把最棘手的「echo/printf 重定向
//! 到真实文件」实现为一个**引号感知 + /dev 设备槽豁免**的 Rust 扫描器，可读性与正确性都更好。

use std::sync::Arc;

use regex::Regex;

/// 已编译的拦截规则：匹配闭包 + 重定向目标工具名 + 教学提示。
///
/// 匹配器用 `Arc<dyn Fn>`（而非直接存 `Regex`）并派生 `Clone`，是为了让「echo 重定向」这类
/// 无法用单个 RE2 正则表达的判定也能以同一套规则结构参与匹配，且装配层可在父/子 Agent 间
/// 共享同一份已编译规则（server 的 `builtin_tools` 与 `builtin_tools_with_pool` 各需一份）。
#[derive(Clone)]
pub struct CompiledRule {
    /// 重定向目标工具名（如 `read_file` / `write_file`）。
    pub tool: String,
    /// 教学提示（解释为何专用工具更合适）。
    pub message: String,
    matches: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

impl CompiledRule {
    /// 由 RE2 正则构造一条规则；正则无效则返回 `None`（调用方决定如何处理）。
    fn regex(tool: &str, message: &str, pattern: &str) -> Option<Self> {
        let re = Regex::new(pattern).ok()?;
        Some(Self {
            tool: tool.into(),
            message: message.into(),
            matches: Arc::new(move |c| re.is_match(c)),
        })
    }

    /// 由任意判定函数构造一条规则（用于 echo 重定向这类复合检测）。
    fn custom<F>(tool: &str, message: &str, f: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        Self {
            tool: tool.into(),
            message: message.into(),
            matches: Arc::new(f),
        }
    }
}

/// 内置默认规则集：把 `cat/head/tail` → `read_file`、`grep/rg` → `grep`、
/// `find/fd -name` → `glob`、`sed -n 'A,Bp'` → `read_file`、`echo/printf > 文件` → `write_file`、
/// `sed -i` → `write_file`、`ls` → `list_files`。
///
/// 刻意只覆盖**核心工具作目标**的命令：`read_file` / `grep` / `glob` / `write_file` /
/// `list_files` 均为始终启用的核心工具，故这套默认规则在任何装配下都安全（不会把模型引向
/// 未注册的工具）。`sed -i` 目标取 `write_file`（而非可选的 `apply_hashline`），避免在未
/// 启用 hashline 时困住模型；教学提示中会同时给出 `apply_hashline`（启用时更合适）。
///
/// 规则集的演进依据实测会话：模型常用的「读文件区间」是 `sed -n 'A,Bp'`（而非 cat），
/// 「原地编辑」是 `sed -i`，故两者与 `ls` 一并纳入。
#[must_use]
pub fn default_compiled() -> Vec<CompiledRule> {
    let mut rules = Vec::new();
    if let Some(r) = CompiledRule::regex(
        "read_file",
        "read_file 提供行号、支持分段读取，并能识别二进制/图片，比 cat/head/tail/less/more 更安全",
        r"^\s*(cat|head|tail|less|more)\s+",
    ) {
        rules.push(r);
    }
    if let Some(r) = CompiledRule::regex(
        "grep",
        "grep 工具尊重 .gitignore 且输出结构化，比 grep/rg/ack 更合适",
        r"^\s*(grep|rg|ripgrep|ag|ack)(\s|$)",
    ) {
        rules.push(r);
    }
    if let Some(r) = CompiledRule::regex(
        "glob",
        "glob 工具尊重 .gitignore 且按文件名模式更快，比 find/fd/locate 更合适",
        r"^\s*(find|fd|locate)\s+.*(-name|-iname|-type|--type|-glob)",
    ) {
        rules.push(r);
    }
    if let Some(r) = CompiledRule::regex(
        "read_file",
        "read_file 支持按行号分段读取（等价 sed -n 'A,Bp'），还能识别二进制/图片，比 sed 更安全",
        r"^\s*sed\s+-n\s+",
    ) {
        rules.push(r);
    }
    if let Some(r) = CompiledRule::regex(
        "list_files",
        "list_files 工具提供结构化目录列表（含大小/修改时间），比 ls 更适合定位文件",
        r"^\s*ls\b",
    ) {
        rules.push(r);
    }
    rules.push(CompiledRule::custom(
        "write_file",
        "write_file 处理编码并提供整文件创建/覆写，比 echo/printf/heredoc 重定向更安全",
        is_write_redirect,
    ));
    if let Some(r) = CompiledRule::regex(
        "write_file",
        "原地编辑请用 apply_hashline（启用时）或 write_file 重写该文件；sed -i 易出转义错误且不留审计",
        r"^\s*sed\s+-i",
    ) {
        rules.push(r);
    }
    rules
}

/// 检查命令是否应被拦截。返回命中的规则（含重定向工具与提示）。
///
/// 同时检查原始命令与 `cd <path> && ...` / `cd <path>; ...` 归一化后的命令，避免前缀包装
/// 绕过（模型常写 `cd src && cat f`）。
#[must_use]
pub fn check<'a>(command: &str, rules: &'a [CompiledRule]) -> Option<&'a CompiledRule> {
    let trimmed = command.trim();
    let stripped = strip_leading_cd(trimmed).unwrap_or_default();
    for candidate in [trimmed, stripped] {
        if candidate.is_empty() {
            continue;
        }
        for rule in rules {
            let matcher = &rule.matches;
            if matcher(candidate) {
                return Some(rule);
            }
        }
    }
    None
}

/// 去掉 `cd <path> && ` / `cd <path>; ` 前缀，返回剩余命令片段（仅字面剥离，不做 shell 展开）。
fn strip_leading_cd(cmd: &str) -> Option<&str> {
    let after_cd = cmd
        .strip_prefix("cd ")
        .or_else(|| cmd.strip_prefix("cd\t"))?;
    let sep = after_cd.find("&&").or_else(|| after_cd.find(';'))?;
    Some(after_cd[sep..].trim_start_matches([' ', '&', ';']))
}

/// 检测 `echo` / `printf` / `cat <<` 把内容重定向到**真实文件**。
///
/// 判定（移植 oh-my-pi echo 重定向规则的语义，规避 RE2 无前瞻的限制）：
/// 1. 命令以 `echo` / `printf` 起头，或为 `cat <<` heredoc；
/// 2. 在引号区域之外存在 `>` / `>>` / `>|` 重定向；
/// 3. 重定向目标是真实文件（排除 `/dev/null|tty|stdout|stderr` 设备槽与 `&N` fd 复制）。
fn is_write_redirect(cmd: &str) -> bool {
    let trimmed = cmd.trim_start();
    let head = trimmed.split_whitespace().next().unwrap_or("");
    let echo_like = matches!(head, "echo" | "printf")
        || trimmed.starts_with("cat <<")
        || trimmed.starts_with("cat<<");
    if !echo_like {
        return false;
    }

    let bytes = cmd.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'>' if !in_single && !in_double => {
                // 跨过可选的第二字符：`>>`（追加）或 `>|`（强制覆盖）。
                let mut j = i + 1;
                if j < bytes.len() && (bytes[j] == b'>' || bytes[j] == b'|') {
                    j += 1;
                }
                // 跳过目标前的空白。
                let mut k = j;
                while k < bytes.len() && matches!(bytes[k], b' ' | b'\t') {
                    k += 1;
                }
                if k >= bytes.len() {
                    break;
                }
                // 目标 token 取到首个空白/分隔符。
                let end = bytes[k..]
                    .iter()
                    .position(|&b| matches!(b, b' ' | b'\t' | b';' | b'|' | b'&' | b'\n'))
                    .map_or(bytes.len(), |p| k + p);
                if is_real_file_target(&cmd[k..end]) {
                    return true;
                }
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// 重定向目标是否为「真实文件」（非设备槽、非 fd 复制、非空）。
fn is_real_file_target(target: &str) -> bool {
    let t = target.trim_matches(|c| c == '"' || c == '\'');
    if t.is_empty() || t.starts_with('&') {
        return false;
    }
    !matches!(t, "/dev/null" | "/dev/tty" | "/dev/stdout" | "/dev/stderr")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit<'a>(cmd: &str, rules: &'a [CompiledRule]) -> Option<&'a str> {
        check(cmd, rules).map(|r| r.tool.as_str())
    }

    #[test]
    fn intercepts_cat_family_to_read_file() {
        let rules = default_compiled();
        assert_eq!(hit("cat src/main.rs", &rules), Some("read_file"));
        assert_eq!(hit("  tail -n 20 log.txt", &rules), Some("read_file"));
        assert_eq!(hit("head -c 100 bin", &rules), Some("read_file"));
        // 裸 cat（无参数）不拦截——避免误伤。
        assert_eq!(hit("cat", &rules), None);
    }

    #[test]
    fn intercepts_grep_family() {
        let rules = default_compiled();
        assert_eq!(hit("grep -rn foo src/", &rules), Some("grep"));
        assert_eq!(hit("rg 'pattern'", &rules), Some("grep"));
        assert_eq!(hit("ripgrep -i foo", &rules), Some("grep"));
    }

    #[test]
    fn intercepts_find_with_name_flags_only() {
        let rules = default_compiled();
        assert_eq!(hit("find . -name '*.rs'", &rules), Some("glob"));
        assert_eq!(hit("find . -type f", &rules), Some("glob"));
        assert_eq!(hit("fd --glob '*.rs'", &rules), Some("glob"));
        // 不带 name/type/glob 标志不拦截（语义不明，可能是 -delete/-exec 链；fd 的 -t 短形亦不在列）。
        assert_eq!(hit("find . -delete", &rules), None);
        assert_eq!(hit("fd -t f", &rules), None);
    }

    #[test]
    fn intercepts_echo_redirect_to_real_file() {
        let rules = default_compiled();
        assert_eq!(hit("echo hi > out.txt", &rules), Some("write_file"));
        assert_eq!(hit("echo hi >> out.txt", &rules), Some("write_file"));
        assert_eq!(hit("printf '%s' foo > /tmp/x", &rules), Some("write_file"));
        assert_eq!(hit("echo hi >| out.txt", &rules), Some("write_file"));
        assert_eq!(hit("echo hi > $OUT", &rules), Some("write_file"));
    }

    #[test]
    fn does_not_intercept_dev_sinks() {
        let rules = default_compiled();
        assert_eq!(hit("echo result > /dev/null", &rules), None);
        assert_eq!(hit("echo done > /dev/null 2>&1", &rules), None);
        assert_eq!(hit("echo x > /dev/stdout", &rules), None);
        assert_eq!(hit("echo x > /dev/tty", &rules), None);
        assert_eq!(hit("echo x > \"/dev/null\"", &rules), None);
    }

    #[test]
    fn still_intercepts_real_paths_resembling_dev_sinks() {
        let rules = default_compiled();
        assert_eq!(hit("echo data > ./dev/null", &rules), Some("write_file"));
        assert_eq!(hit("echo data > /devices/x", &rules), Some("write_file"));
    }

    #[test]
    fn quote_aware_redirect_in_string_is_not_a_redirect() {
        let rules = default_compiled();
        // 引号内的 `>` 不是重定向。
        assert_eq!(hit("echo \"a -> b\"", &rules), None);
        assert_eq!(hit("printf 'use 2>&1'", &rules), None);
        // 引号内有箭头、引号外仍有真重定向——应拦截。
        assert_eq!(hit("echo \"a -> b\" > out.txt", &rules), Some("write_file"));
    }

    #[test]
    fn keeps_scanning_past_dev_sink_to_later_real_redirect() {
        let rules = default_compiled();
        assert_eq!(
            hit("echo data > /dev/null > out.txt", &rules),
            Some("write_file")
        );
        assert_eq!(
            hit("printf x > /dev/stdout >> real.txt", &rules),
            Some("write_file")
        );
    }

    #[test]
    fn cd_prefix_does_not_bypass_interception() {
        let rules = default_compiled();
        assert_eq!(hit("cd src && cat main.rs", &rules), Some("read_file"));
        assert_eq!(hit("cd src; grep foo .", &rules), Some("grep"));
        assert_eq!(
            hit("cd src && echo hi > out.txt", &rules),
            Some("write_file")
        );
    }

    #[test]
    fn intercepts_sed_n_read_ranges_to_read_file() {
        let rules = default_compiled();
        // 实测高频：模型用 sed -n 'A,Bp' 读文件区间。
        assert_eq!(
            hit("sed -n '3425,3440p' Cargo.lock", &rules),
            Some("read_file")
        );
        assert_eq!(
            hit("sed -n '55,75p' crates/config/src/config.rs", &rules),
            Some("read_file")
        );
        assert_eq!(hit("sed -n 1,15p file", &rules), Some("read_file"));
        assert_eq!(hit("sed -n '574p' main.rs", &rules), Some("read_file"));
        assert_eq!(hit("sed -n '/foo/,/bar/p' x.rs", &rules), Some("read_file"));
        // cd 前缀不能绕过。
        assert_eq!(
            hit("cd src && sed -n '1,5p' main.rs", &rules),
            Some("read_file")
        );
    }

    #[test]
    fn intercepts_sed_i_inplace_edit_to_write_file() {
        let rules = default_compiled();
        // 实测高频：模型用 sed -i 做原地编辑。
        assert_eq!(hit("sed -i 's/x/y/' file.txt", &rules), Some("write_file"));
        assert_eq!(
            hit("sed -i '63,117d' src/lib.rs", &rules),
            Some("write_file")
        );
        assert_eq!(hit("sed -i.bak 's/a/b/' f", &rules), Some("write_file"));
        // 不带 -n/-i 的 sed（过滤管道，输出到 stdout）不拦截。
        assert_eq!(hit("sed 's/x/y/' | sort", &rules), None);
    }

    #[test]
    fn intercepts_ls_to_list_files() {
        let rules = default_compiled();
        assert_eq!(hit("ls -la", &rules), Some("list_files"));
        assert_eq!(hit("ls dist", &rules), Some("list_files"));
        assert_eq!(hit("ls .. | head -30", &rules), Some("list_files"));
        // 避免误伤：非 ls 开头的命令不受影响。
        assert_eq!(hit("lsblk", &rules), None);
    }

    #[test]
    fn leaves_unrelated_commands_alone() {
        let rules = default_compiled();
        assert_eq!(hit("cargo build", &rules), None);
        assert_eq!(hit("git status", &rules), None);
        assert_eq!(hit("echo hello world", &rules), None);
        assert_eq!(hit("wc -l Cargo.lock", &rules), None);
        assert_eq!(
            hit("python3 -c 'import os; print(os.listdir())'", &rules),
            None
        );
    }
}
