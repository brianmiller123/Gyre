//! `${ENV}` 变量展开。

/// 将 `${VAR}` 形式的占位符替换为环境变量值；未设置则替换为空串。
///
/// 用于 `api_key`、`server.auth_token` 等敏感字段，避免明文落盘。
#[must_use]
pub fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                let value = std::env::var(name).unwrap_or_default();
                out.push_str(&value);
                rest = &after[end + 1..];
            }
            None => {
                // 无闭合 `}`，原样输出从 `${` 起的剩余。
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
#[allow(unsafe_code)] // 测试需 env 变更（edition 2024 为 unsafe），仅测试代码
mod tests {
    use super::expand_env;

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
}
