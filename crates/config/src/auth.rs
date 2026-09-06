//! `auth.toml` 持久化凭据存储与 API key 三段回退解析。
//!
//! 解析优先级（高 → 低，见 [`resolve`]）：
//! 1. config `api_key`（`${ENV}` 已展开，非空即用）；
//! 2. `<config_dir>/auth.toml`（`provider = "sk-..."` 扁平表）；
//! 3. 环境变量 `GYRE_<PROVIDER 大写，连字符→下划线>_API_KEY`。
//!
//! provider 名取线协议族 [`agent_core::Api`] 的 serde 名（`Api::as_str()`）：
//! `"anthropic-messages"` / `"openai-responses"` / `"openai-completions"` /
//! `"deepseek"` / `"zai"` / `"google-generative-ai"` / `"ollama-chat"`。
//! `ModelProfile` 本身无 provider 字段，故以线协议族为粒度共享凭据。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// provider 名归一化：trim + 小写（auth.toml key 与查找统一走此处，大小写不敏感）。
pub(crate) fn normalize_provider(provider: &str) -> String {
    provider.trim().to_lowercase()
}

/// provider 对应的兜底环境变量名：`GYRE_` + provider 名直接大写（连字符转下划线）+ `_API_KEY`。
///
/// 如 `openai-completions` → `GYRE_OPENAI_COMPLETIONS_API_KEY`，`openai` → `GYRE_OPENAI_API_KEY`。
#[must_use]
pub fn env_var_name(provider: &str) -> String {
    format!(
        "GYRE_{}_API_KEY",
        normalize_provider(provider)
            .replace('-', "_")
            .to_uppercase()
    )
}

/// `<config_dir>/auth.toml` 路径。
#[must_use]
pub fn auth_path(config_dir: &Path) -> PathBuf {
    config_dir.join("auth.toml")
}

/// API key 凭据存储（auth.toml 的内存映像）。
///
/// key = 归一化 provider 名（即线协议族 [`agent_core::Api`] 的 serde 名小写，
/// 如 `openai-completions`），value = 明文 API key（由调用方负责不落日志）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthStore(pub HashMap<String, String>);

impl AuthStore {
    /// 查 provider 的 key（查找 key 同样归一化，大小写不敏感）。
    #[must_use]
    pub fn get(&self, provider: &str) -> Option<&str> {
        self.0
            .get(normalize_provider(provider).as_str())
            .map(String::as_str)
    }
}

/// 加载 `<config_dir>/auth.toml` 为 [`AuthStore`]。
///
/// 文件缺失 → 空 store（首次运行常态）；读取失败或解析失败 → `tracing::warn` + 空
/// store（永不 panic；凭据缺失只影响建连，不应阻断启动）。
#[must_use]
pub fn load(config_dir: &Path) -> AuthStore {
    let path = auth_path(config_dir);
    let src = match std::fs::read_to_string(&path) {
        Ok(src) => src,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AuthStore::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "auth.toml 不可读，忽略凭据存储");
            return AuthStore::default();
        }
    };
    match toml::from_str::<HashMap<String, String>>(&src) {
        Ok(map) => AuthStore(
            map.into_iter()
                .map(|(k, v)| (normalize_provider(&k), v))
                .collect(),
        ),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "auth.toml 解析失败，忽略凭据存储");
            AuthStore::default()
        }
    }
}

/// API key 三段回退解析。
///
/// 顺序：config 值非空（`config_key_empty == false` 且 `profile_value` 非空——
/// `${ENV}` 已在上游展开，展开后为空视同未配置）优先；否则 auth.toml（`store`）；
/// 否则环境变量 [`env_var_name`]。三处皆空 → `None`。
#[must_use]
pub fn resolve(
    config_key_empty: bool,
    profile_value: &str,
    store: &AuthStore,
    provider: &str,
) -> Option<String> {
    // 1) config `api_key` 优先。
    if !config_key_empty && !profile_value.is_empty() {
        return Some(profile_value.to_owned());
    }
    // 2) auth.toml 持久凭据（空串条目视同未配置，继续回退）。
    if let Some(key) = store.get(provider) {
        if !key.is_empty() {
            return Some(key.to_owned());
        }
    }
    // 3) 环境变量兜底（非 UTF-8 视同未设置）。
    match std::env::var(env_var_name(provider)) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// 写入/更新某 provider 的 key 到 `<config_dir>/auth.toml`（保留其他 provider 条目）。
///
/// 目录不存在则创建；新文件以 0600 权限落地，已存在则保留原权限位；先写同目录
/// 临时文件再 `rename` 原子替换。落盘按 key 排序，保证内容稳定可 diff。
///
/// # Errors
/// 目录创建 / 文件写入 / rename 失败时返回 [`std::io::Error`]。
pub fn save(config_dir: &Path, provider: &str, key: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir)?;

    // 整表读入后改写（缺失/畸形 → 空表覆盖，语义与 load 一致）。
    let mut store = load(config_dir);
    store.0.insert(normalize_provider(provider), key.to_owned());
    // HashMap 序列化顺序不定，用 BTreeMap 按键排序落盘。
    let sorted: std::collections::BTreeMap<&str, &str> = store
        .0
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let body = toml::to_string(&sorted)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    atomic_write_0600(&auth_path(config_dir), body.as_bytes())
}

/// 0600 原子写：临时文件以 0600 新建（rename 后即正式文件权限，杜绝宽权限中间态），
/// 已存在文件保留其权限位（用户可能自行放宽/收紧过），同目录临时文件 + `rename` 原子替换。
pub(crate) fn atomic_write_0600(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let tmp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(body)?;
    drop(f);

    if let Ok(meta) = std::fs::metadata(path) {
        let perms = meta.permissions();
        let _ = std::fs::set_permissions(&tmp, perms);
    }

    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录（进程号 + 纳秒防撞，风格同 config.rs / approval.rs 测试）。
    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gyre-auth-{}-{tag}-{:#x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn auth_path_layout() {
        assert_eq!(
            auth_path(Path::new("/cfg")),
            PathBuf::from("/cfg/auth.toml")
        );
    }

    #[test]
    fn env_var_name_mapping() {
        assert_eq!(
            env_var_name("openai-completions"),
            "GYRE_OPENAI_COMPLETIONS_API_KEY"
        );
        assert_eq!(
            env_var_name("anthropic-messages"),
            "GYRE_ANTHROPIC_MESSAGES_API_KEY"
        );
        assert_eq!(env_var_name("OpenAI"), "GYRE_OPENAI_API_KEY");
        assert_eq!(env_var_name("  DeepSeek "), "GYRE_DEEPSEEK_API_KEY");
    }

    #[test]
    fn resolve_prefers_config_value() {
        let mut map = HashMap::new();
        map.insert("openai".to_string(), "sk-store".to_string());
        let store = AuthStore(map);
        // config 非空 → 压过 auth.toml。
        assert_eq!(
            resolve(false, "sk-config", &store, "openai"),
            Some("sk-config".to_string())
        );
    }

    #[test]
    fn resolve_falls_back_to_auth_toml() {
        let mut map = HashMap::new();
        map.insert("openai".to_string(), "sk-store".to_string());
        let store = AuthStore(map);
        // config 空（config_key_empty 或 ${UNSET} 展开为空）→ 回退 auth.toml。
        assert_eq!(
            resolve(true, "", &store, "openai"),
            Some("sk-store".to_string())
        );
        assert_eq!(
            resolve(false, "", &store, "openai"),
            Some("sk-store".to_string())
        );
        // auth.toml 条目为空串 → 继续回退（此处 env 也未设）→ None。
        let mut map = HashMap::new();
        map.insert("zz-none-provider".to_string(), String::new());
        assert_eq!(resolve(true, "", &AuthStore(map), "zz-none-provider"), None);
    }
    #[test]
    #[allow(unsafe_code)] // env.rs 同款测试惯例：edition 2024 的 env 变更需 unsafe
    fn resolve_env_fallback_and_all_empty() {
        // SAFETY: 测试专用环境变量，本测试二进制独占，无并发竞争（仓库既有 env 测试惯例）。
        const VAR: &str = "GYRE_TEST_AUTH_ENV_API_KEY";
        unsafe { std::env::set_var(VAR, "sk-from-env") };
        let empty = AuthStore::default();
        // provider "test-auth-env" → 环境变量 GYRE_TEST_AUTH_ENV_API_KEY。
        assert_eq!(
            resolve(true, "", &empty, "test-auth-env"),
            Some("sk-from-env".to_string())
        );
        unsafe { std::env::remove_var(VAR) };
        // 三处全空 → None（生僻 provider 名，宿主机不可能有同名变量）。
        assert_eq!(resolve(true, "", &empty, "zz-no-such-provider-xyz"), None);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tmp_dir("missing");
        assert_eq!(load(&dir).get("openai"), None);
    }

    #[test]
    fn load_malformed_returns_empty_without_panic() {
        let dir = tmp_dir("malformed");
        let path = auth_path(&dir);
        // 畸形 TOML → warn + 空。
        std::fs::write(&path, "this is [ not toml").unwrap();
        assert!(load(&dir).0.is_empty());
        // 值类型非字符串同样解析失败 → 空。
        std::fs::write(&path, "openai = 42\n").unwrap();
        assert!(load(&dir).0.is_empty());
    }

    #[test]
    fn load_normalizes_keys_case_insensitive() {
        let dir = tmp_dir("normalize");
        std::fs::write(auth_path(&dir), "OpenAI-Completions = \"sk-o\"\n").unwrap();
        let store = load(&dir);
        assert_eq!(store.get("openai-completions"), Some("sk-o"));
        assert_eq!(store.get("  OPENAI-COMPLETIONS "), Some("sk-o"));
    }

    #[test]
    fn save_then_load_roundtrip_and_preserves_other_providers() {
        let dir = tmp_dir("roundtrip");
        // provider 名带大小写/空格 → 归一化落盘。
        save(&dir, "Anthropic-Messages", "sk-a").unwrap();
        save(&dir, "deepseek", "sk-b").unwrap();
        let store = load(&dir);
        assert_eq!(store.get("anthropic-messages"), Some("sk-a"));
        assert_eq!(store.get("DEEPSEEK"), Some("sk-b"));
        // 更新一个不影响另一个。
        save(&dir, "anthropic-messages", "sk-a2").unwrap();
        let store = load(&dir);
        assert_eq!(store.get("anthropic-messages"), Some("sk-a2"));
        assert_eq!(store.get("deepseek"), Some("sk-b"));
    }

    #[cfg(unix)]
    #[test]
    fn save_new_file_gets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("perm-new");
        save(&dir, "openai", "sk-1").unwrap();
        let meta = std::fs::metadata(auth_path(&dir)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("perm-keep");
        let path = auth_path(&dir);
        save(&dir, "openai", "sk-1").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        save(&dir, "openai", "sk-2").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
        assert_eq!(load(&dir).get("openai"), Some("sk-2"));
        // 无临时文件残留。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "应无临时文件残留: {leftovers:?}");
    }
}
