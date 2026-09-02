//! 跨平台路径与编译守卫（§12）。
//!
//! 平台差异集中在本模块，业务路径一律通过 [`config_dir`] 等抽象获取目录，
//! 禁止硬编码 `/` 或 `\`。

use std::path::PathBuf;

/// 返回用户级配置目录（跨平台）。
///
/// - Linux: `~/.config/agent`
/// - Windows: `%APPDATA%\agent`
/// - macOS: `~/Library/Application Support/agent`
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("agent"))
}

/// 返回项目级配置目录候选（`.agent/`，相对工作区根）。
#[must_use]
pub const fn project_config_dir_name() -> &'static str {
    ".agent"
}

/// 仅支持 unix 与 windows 平台（macOS 属于 unix）。
#[cfg(not(any(unix, windows)))]
compile_error!("agent-project 仅支持 unix 与 windows 平台");

// ──────────────────────────────────────────────────────────────────────────────
// 命令子进程 UTF-8 locale 保障
// ──────────────────────────────────────────────────────────────────────────────
//
// 整条捕获链路（run_command / run_pty_command → String::from_utf8_lossy → JSON/WS）
// 均假设输出为 UTF-8。若命令子进程的有效 locale 非 UTF-8（如服务端由 systemd / 容器 /
// 后台启动器拉起而未继承 UTF-8 locale），中文程序会以 GBK/GB18030 等编码输出，
// 经 lossy 解码即变成乱码。这里在 spawn 前按需注入 UTF-8 locale 兜底。

/// 判断一个 locale 规约是否要求 UTF-8 编码。
///
/// 匹配 `UTF-8` / `UTF8`（大小写不敏感），忽略修饰符（如 `@cjk`）与编码前缀。
fn is_utf8_locale_spec(spec: &str) -> bool {
    let up = spec.to_ascii_uppercase();
    up.contains("UTF-8") || up.contains("UTF8")
}

/// 纯函数版：据给定的 locale 变量值判断**有效** locale 是否 UTF-8。
///
/// 按 `LC_ALL > LC_CTYPE > LANG` 优先级取首个**已设置且非空**的值判断；空值视同未设置
/// （继续降级）。全部未设置 / 为空时返回 `false`（等价于 `C`/`POSIX`，非 UTF-8）。
///
/// 抽成纯函数以便在 `#![forbid(unsafe_code)]` 的本 crate 内无需改 env 即可单测。
fn effective_utf8_from(lc_all: Option<&str>, lc_ctype: Option<&str>, lang: Option<&str>) -> bool {
    for val in lc_all.into_iter().chain(lc_ctype).chain(lang) {
        let trimmed = val.trim();
        if trimmed.is_empty() {
            continue;
        }
        return is_utf8_locale_spec(trimmed);
    }
    false
}

/// 当前进程环境的有效 locale 是否已解析为 UTF-8（读取真实 env）。
fn env_effective_utf8() -> bool {
    effective_utf8_from(
        std::env::var("LC_ALL").ok().as_deref(),
        std::env::var("LC_CTYPE").ok().as_deref(),
        std::env::var("LANG").ok().as_deref(),
    )
}

/// 当继承的环境未解析为 UTF-8 时，返回应注入命令子进程的 UTF-8 locale 值；已是 UTF-8 则 `None`。
///
/// 固定使用 `C.UTF-8`（现代 glibc ≥2.35 与 musl 均内置；仅影响编码、语言中立），确保命令
/// 子进程始终以 UTF-8 输出。调用方在 spawn 前据此 `cmd.env("LC_ALL"/"LANG", loc)`。
#[must_use]
pub fn forced_utf8_locale() -> Option<&'static str> {
    if env_effective_utf8() {
        None
    } else {
        Some("C.UTF-8")
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// LLM 请求 User-Agent（与 OMP 对齐）
// ──────────────────────────────────────────────────────────────────────────────
//
// 默认 LLM 请求 User-Agent 与 oh-my-pi（OMP）线上一致：
// `pi/<version> (<platform> <release>; <arch>)`（见 upstream
// `packages/ai/src/providers/openai-codex-responses.ts` 的
// `User-Agent: pi/${packageJson.version} (${os.platform()} ${os.release()}; ${os.arch()})`）。
// 部分上游/网关按 UA 识别客户端，保持同一指纹可避免行为差异。

/// OMP 版本号（锁定自 `third/oh-my-pi/packages/ai/package.json`；升级上游时同步更新）。
pub const OMP_VERSION: &str = "17.2.3";

/// 映射为 Node `os.platform()` 的取值（`linux` / `darwin` / `win32`）。
fn omp_platform() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// 映射为 Node `os.arch()` 的取值（`x64` / `arm64` / `ia32`）。
fn omp_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/// 内核版本号（对齐 Node `os.release()` / `uname -r`）。
///
/// Linux 读 `/proc/sys/kernel/osrelease`（无依赖、无子进程）；其余平台回退到
/// 编译期 OS 名（UA 格式仍然合法，仅 release 段非内核版本）。
fn kernel_release() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    std::env::consts::OS.to_string()
}

/// 默认 LLM 请求 User-Agent：`pi/<version> (<platform> <release>; <arch>)`，
/// 与 OMP 线上格式完全一致。配置文件 `user_agent` 键可覆盖（见 `agent-config`）。
#[must_use]
pub fn default_llm_user_agent() -> String {
    format!(
        "pi/{OMP_VERSION} ({} {}; {})",
        omp_platform(),
        kernel_release(),
        omp_arch()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_utf8_specs() {
        assert!(is_utf8_locale_spec("en_US.UTF-8"));
        assert!(is_utf8_locale_spec("zh_CN.UTF-8"));
        assert!(is_utf8_locale_spec("C.UTF8"));
        assert!(is_utf8_locale_spec("en_US.utf-8"));
        assert!(!is_utf8_locale_spec("C"));
        assert!(!is_utf8_locale_spec("POSIX"));
        assert!(!is_utf8_locale_spec("zh_CN.GB18030"));
    }

    #[test]
    fn precedence_lc_all_wins() {
        // LC_ALL 非 UTF-8 覆盖 UTF-8 的 LANG。
        assert!(!effective_utf8_from(Some("C"), None, Some("en_US.UTF-8")));
        // LC_ALL UTF-8 即足够。
        assert!(effective_utf8_from(Some("C.UTF-8"), None, None));
    }

    #[test]
    fn precedence_lc_ctype_then_lang() {
        assert!(effective_utf8_from(None, Some("zh_CN.UTF-8"), Some("C")));
        assert!(!effective_utf8_from(None, Some("C"), Some("en_US.UTF-8")));
        assert!(effective_utf8_from(None, None, Some("en_US.UTF-8")));
    }

    #[test]
    fn empty_or_unset_is_not_utf8() {
        // 空值视同未设置，降级到下一优先级。
        assert!(effective_utf8_from(Some(""), Some(""), Some("en_US.UTF-8")));
        // 全部空 / 未设置 → 非 UTF-8（等价 C/POSIX）。
        assert!(!effective_utf8_from(Some(""), None, None));
        assert!(!effective_utf8_from(None, None, None));
    }

    #[test]
    fn default_llm_user_agent_matches_omp_format() {
        // 与 OMP 线上格式一致：`pi/<version> (<platform> <release>; <arch>)`。
        let ua = default_llm_user_agent();
        assert!(
            ua.starts_with(&format!("pi/{OMP_VERSION} (")),
            "UA 应以 OMP 版本开头: {ua}"
        );
        assert!(ua.ends_with(')'), "UA 应以右括号结尾: {ua}");
        // `(platform release; arch)` 结构：括号内恰含一个分号，分号两侧均非空。
        let inner = &ua[ua.find('(').unwrap() + 1..ua.rfind(')').unwrap()];
        let (plat_release, arch) = inner.split_once(';').expect("应含 `;` 分隔: {ua}");
        assert!(!plat_release.trim().is_empty() && !arch.trim().is_empty());
        assert_eq!(plat_release.split_whitespace().count(), 2);
    }
}
