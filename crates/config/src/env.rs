//! `${ENV}` 变量展开（H36：dotenv 回落 + `${VAR:-default}` 默认值）。

/// 将 `${VAR}` / `${VAR:-default}` 形式的占位符替换为环境变量值。
///
/// 查找顺序（对齐 oh-my-pi `lookupEnvValue`）：
/// 1. 真实进程环境（`std::env::var`，含空串——`${VAR}` 空串即空串）；
/// 2. 进程级 dotenv（见 [`crate::dotenv`]；`.env` 只补缺，不覆盖 shell 导出的值）。
///
/// 展开规则：
/// - `${VAR}`：取值；**未设置**时替换为空串（Gyre 既有行为，保持向后兼容）。
/// - `${VAR:-default}`：POSIX `:-` 语义——未设置**或为空**时用 `default`，否则用值。
/// - `${VAR:-}`：显式空默认（等价于未设置即空串）。
/// - 名字非法（空 / 含 `:`、`}` 等）或无闭合 `}`：原样输出。
///
/// 用于 `api_key`、`server.auth_token` 等敏感字段，避免明文落盘。
#[must_use]
pub fn expand_env(input: &str) -> String {
    expand_env_with(input, &|name| std::env::var(name).ok())
}

/// 以自定义查找函数展开（测试与显式注入用；dotenv 仍参与回落）。
#[must_use]
pub fn expand_env_with(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // 无闭合 `}`，原样输出从 `${` 起的剩余。
            out.push_str(&rest[start..]);
            return out;
        };
        let body = &after[..end];
        let (name, default) = split_name_default(body);
        if name.is_empty() || name.contains(['}', ':', '$']) {
            // 非法名字：整个占位符原样输出（不做替换，也不当默认值处理）。
            out.push_str(&rest[start..start + 2 + end + 1]);
            rest = &after[end + 1..];
            continue;
        }
        let resolved = lookup(name).or_else(|| dotenv_lookup(name));
        match (resolved, default) {
            (Some(v), Some(d)) if v.is_empty() => out.push_str(d),
            (Some(v), _) => out.push_str(&v),
            (None, Some(d)) => out.push_str(d),
            (None, None) => {
                // 未设置且无默认：保持既有「空串」语义。
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// 拆 `${VAR:-default}` 的 `NAME` 与 `:-` 之后的默认值（无 `:-` 时默认值为 `None`）。
fn split_name_default(body: &str) -> (&str, Option<&str>) {
    match body.find(":-") {
        Some(idx) => (&body[..idx], Some(&body[idx + 2..])),
        None => (body, None),
    }
}

/// dotenv 表回落（未安装时为 `None`）。
fn dotenv_lookup(name: &str) -> Option<String> {
    crate::dotenv::dotenv().and_then(|d| d.get(name).map(str::to_string))
}

#[cfg(test)]
#[allow(unsafe_code)] // 测试需 env 变更（edition 2024 为 unsafe），仅测试代码
mod tests {
    use super::{expand_env, expand_env_with};

    #[test]
    fn expands_known_var() {
        // edition 2024: env 变更需 unsafe。
        unsafe { std::env::set_var("AGENT_TEST_KEY", "sk-test-123") };
        assert_eq!(expand_env("bearer ${AGENT_TEST_KEY}"), "bearer sk-test-123");
    }

    #[test]
    fn missing_var_becomes_empty() {
        unsafe { std::env::remove_var("AGENT_NOPE") };
        assert_eq!(expand_env("${AGENT_NOPE}"), "");
    }

    #[test]
    fn unclosed_is_passthrough() {
        assert_eq!(expand_env("abc${UNCLOSED"), "abc${UNCLOSED");
    }

    #[test]
    fn default_value_applies_when_unset_or_empty() {
        let lookup = |name: &str| -> Option<String> {
            match name {
                "SET" => Some("value".into()),
                "EMPTY" => Some(String::new()),
                _ => None,
            }
        };
        // POSIX `:-`：未设置 → 默认；空 → 默认；有值 → 值。
        assert_eq!(
            expand_env_with("${MISSING:-fallback}", &lookup),
            "fallback",
            "未设置取默认"
        );
        assert_eq!(
            expand_env_with("${EMPTY:-fallback}", &lookup),
            "fallback",
            "空值也取默认"
        );
        assert_eq!(expand_env_with("${SET:-fallback}", &lookup), "value");
        // 显式空默认 → 空串；无 `:-` 的空值 → 空串（既有语义）。
        assert_eq!(expand_env_with("${EMPTY:-}", &lookup), "");
        assert_eq!(expand_env_with("${EMPTY}", &lookup), "");
        // 默认值里的 `}` 不参与（第一个 `}` 收尾）——防越界注入。
        assert_eq!(expand_env_with("key=${MISSING:-a}b}", &lookup), "key=ab}");
        // 非法名字（含 `$`）原样保留。
        assert_eq!(expand_env_with("${A$B}", &lookup), "${A$B}");
    }

    #[test]
    fn multiple_placeholders_in_one_string() {
        let lookup = |name: &str| -> Option<String> {
            match name {
                "HOST" => Some("example.com".into()),
                _ => None,
            }
        };
        assert_eq!(
            expand_env_with("https://${HOST}:${PORT:-443}/v1", &lookup),
            "https://example.com:443/v1"
        );
    }
}
