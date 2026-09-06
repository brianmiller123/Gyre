//! `oauth.toml` 持久化 OAuth 凭据存储（对齐 oh-my-pi AuthStorage 的凭据语义）。
//!
//! 与 [`crate::auth`]（API key 扁平表）并列的第二凭据面：每个 provider 一张表，
//! 存 `OAuthCredentials`（refresh/access/expires + 身份附加字段）。
//!
//! 解析优先级（高 → 低，整体链见 `auth::resolve`）：config `api_key` →
//! **oauth.toml（有效 token）** → auth.toml → `GYRE_*` env。OAuth 优先于
//! auth.toml 对齐 omp 语义（login 会覆写同 provider 的 api_key 凭据）。
//!
//! provider 键与 auth.toml 同规：trim + 小写的线协议族名（`anthropic-messages` 等）。
//! 文件 0600 原子写；解析失败 warn + 空表（凭据缺失只影响建连，不阻断启动）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::auth::normalize_provider;

/// 统一 OAuth 凭据（omp `OAuthCredentials` 的 Rust 镜像，字段名保持 snake_case 落盘）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredentials {
    /// 刷新令牌；恒等刷新型流（copilot 类）refresh == access。
    #[serde(default)]
    pub refresh: String,
    /// 访问令牌；对「铸长效 key」型流（zai 等）即最终 API key。
    pub access: String,
    /// 过期时刻（epoch 毫秒）；长效 key 用「远未来」（[`OAuthCredentials::NEVER_EXPIRES`]）。
    pub expires: i64,
    /// 企业自建端点（GHES 等变体流）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_url: Option<String>,
    /// Google Code Assist 项目 id（gemini 系）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// 登录账号邮箱（身份展示用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// 账号 / 组织 id（登录时捕获一次）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    /// 交互登录时刻（epoch 毫秒）；刷新保留，用于「绝对授权寿命」重登提醒（anthropic 30 天）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_at: Option<i64>,
    /// token 端点（MCP OAuth 登录时捕获，供后续刷新；omp `MCPAuthConfig.tokenUrl` 语义）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_url: Option<String>,
    /// OAuth client id（DCR 动态注册产出或手工配置）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// OAuth client secret（DCR 产出；公开客户端通常为空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// RFC 8707 资源指示（MCP server 受众）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

impl OAuthCredentials {
    /// 「永不过期」哨兵（8.64e15 ms ≈ 27.5 万年，与 omp `NEVER_EXPIRES` 一致）。
    pub const NEVER_EXPIRES: i64 = 8_640_000_000_000_000;

    /// 提前量：距过期不足 60s 视同过期（omp `OAUTH_REFRESH_SKEW_MS`）。
    pub const REFRESH_SKEW_MS: i64 = 60_000;

    #[must_use]
    pub fn is_expired(&self, now_ms: i64) -> bool {
        now_ms + Self::REFRESH_SKEW_MS >= self.expires
    }

    /// 刷新结果合并：以新凭据为主体，但 **org/orgName/authorizedAt/enterpriseUrl/
    /// projectId 保旧**——刷新端点不回传这些字段（omp：登录时捕获一次，refresh 永不改写）；
    /// 新值非空才覆盖 account_id/email（部分流刷新会带身份，部分不带）。
    #[must_use]
    pub fn merge_refreshed(&self, fresh: OAuthCredentials) -> OAuthCredentials {
        let mut out = fresh;
        if out.org_id.is_none() {
            out.org_id = self.org_id.clone();
        }
        if out.org_name.is_none() {
            out.org_name = self.org_name.clone();
        }
        if out.authorized_at.is_none() {
            out.authorized_at = self.authorized_at;
        }
        if out.token_url.is_none() {
            out.token_url = self.token_url.clone();
        }
        if out.client_id.is_none() {
            out.client_id = self.client_id.clone();
        }
        if out.client_secret.is_none() {
            out.client_secret = self.client_secret.clone();
        }
        if out.resource.is_none() {
            out.resource = self.resource.clone();
        }
        if out.enterprise_url.is_none() {
            out.enterprise_url = self.enterprise_url.clone();
        }
        if out.project_id.is_none() {
            out.project_id = self.project_id.clone();
        }
        if out.account_id.is_none() {
            out.account_id = self.account_id.clone();
        }
        if out.email.is_none() {
            out.email = self.email.clone();
        }
        out
    }
}

/// OAuth 凭据存储（oauth.toml 的内存映像）。key = 归一化 provider 名。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OAuthStore(pub BTreeMap<String, OAuthCredentials>);

impl OAuthStore {
    #[must_use]
    pub fn get(&self, provider: &str) -> Option<&OAuthCredentials> {
        self.0.get(&normalize_provider(provider))
    }

    pub fn insert(&mut self, provider: &str, creds: OAuthCredentials) {
        self.0.insert(normalize_provider(provider), creds);
    }
}

/// `<config_dir>/oauth.toml` 路径。
#[must_use]
pub fn oauth_path(config_dir: &Path) -> PathBuf {
    config_dir.join("oauth.toml")
}

/// 加载 `<config_dir>/oauth.toml`。缺失 → 空表；读取/解析失败 → `tracing::warn` + 空表。
#[must_use]
pub fn load(config_dir: &Path) -> OAuthStore {
    let path = oauth_path(config_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return OAuthStore::default();
    };
    match toml::from_str::<OAuthFile>(&text) {
        Ok(file) => OAuthStore(file.providers),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "oauth.toml 解析失败，按空表处理");
            OAuthStore::default()
        }
    }
}

/// 落盘形态：`[providers.<name>]` 子表，避免 provider 名与未来顶层键冲突。
#[derive(Debug, Default, Serialize, Deserialize)]
struct OAuthFile {
    #[serde(default)]
    providers: BTreeMap<String, OAuthCredentials>,
}

/// 写入/更新某 provider 的 OAuth 凭据（保留其他条目）。
///
/// 目录不存在则创建；新文件 0600，已存在保留权限位；同目录临时文件 + `rename` 原子替换；
/// 条目按 provider 名排序（内容稳定可 diff）。
///
/// # Errors
/// 目录创建 / 序列化 / 写入 / rename 失败时返回 [`std::io::Error`]。
pub fn save(
    config_dir: &Path,
    provider: &str,
    creds: &OAuthCredentials,
) -> std::io::Result<PathBuf> {
    let mut store = load(config_dir);
    store.insert(provider, creds.clone());
    write_all(config_dir, &store)
}

/// 删除某 provider 的 OAuth 凭据条目。返回是否存在过（供 CLI 提示）。
///
/// # Errors
/// 读改写过程 IO 失败时返回 [`std::io::Error`]。
pub fn remove(config_dir: &Path, provider: &str) -> std::io::Result<bool> {
    let mut store = load(config_dir);
    let existed = store.0.remove(&normalize_provider(provider)).is_some();
    if existed {
        write_all(config_dir, &store)?;
    }
    Ok(existed)
}

fn write_all(config_dir: &Path, store: &OAuthStore) -> std::io::Result<PathBuf> {
    let path = oauth_path(config_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string(&OAuthFile {
        providers: store.0.clone(),
    })
    .map_err(std::io::Error::other)?;
    crate::auth::atomic_write_0600(&path, body.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(access: &str, expires: i64) -> OAuthCredentials {
        OAuthCredentials {
            refresh: "r".into(),
            access: access.into(),
            expires,
            org_id: Some("org-1".into()),
            authorized_at: Some(111),
            ..Default::default()
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gyre-oauth-store-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn roundtrip_preserves_other_providers_and_mode() {
        let dir = tmpdir("rt");
        save(&dir, "Anthropic-Messages", &creds("a1", 100)).unwrap();
        save(&dir, "zai", &creds("z1", 200)).unwrap();
        // 大小写归一取回。
        let store = load(&dir);
        assert_eq!(store.get("anthropic-messages").unwrap().access, "a1");
        assert_eq!(store.get("ZAI").unwrap().expires, 200);
        // 权限位 0600。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(oauth_path(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "oauth.toml 必须 0600");
        }
        // 删除其一，另一保留。
        assert!(remove(&dir, "zai").unwrap());
        assert!(!remove(&dir, "zai").unwrap(), "二次删除应报不存在");
        let store = load(&dir);
        assert!(store.get("zai").is_none());
        assert!(store.get("anthropic-messages").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_falls_back_to_empty() {
        let dir = tmpdir("bad");
        std::fs::write(oauth_path(&dir), "(((( not toml").unwrap();
        assert_eq!(load(&dir), OAuthStore::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expiry_skew_and_merge_keeps_org_fields() {
        let now = 1_000_000;
        let mut c = creds("a", now + 30_000); // 距过期 30s < 60s 提前量
        assert!(c.is_expired(now), "60s 提前量内视同过期");
        c.expires = now + 61_000;
        assert!(!c.is_expired(now));

        let fresh = OAuthCredentials {
            refresh: "r2".into(),
            access: "a2".into(),
            expires: now + 3_600_000,
            email: Some("new@x".into()),
            ..Default::default()
        };
        let merged = c.merge_refreshed(fresh);
        assert_eq!(merged.org_id.as_deref(), Some("org-1"), "org 保旧");
        assert_eq!(merged.authorized_at, Some(111), "authorizedAt 保旧");
        assert_eq!(merged.email.as_deref(), Some("new@x"), "新身份非空才覆盖");
        assert_eq!(merged.refresh, "r2");
    }
}
