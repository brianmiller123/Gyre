//! 管线级密钥双向脱敏（移植 oh-my-pi `packages/coding-agent/src/secrets/`）。
//!
//! 设计（对齐 omp 的 transform 时机，见 `SecretsObfuscator` 各方法文档）：
//! - **会话上下文存储明文**，provider 边界脱敏：本地 transcript / 压缩 / 显示永远可用，
//!   密钥字节从不越过进程边界（omp `obfuscateProviderContext` 同款语义）。
//! - **出向 transform**：用户消息文本、工具结果文本、assistant 重放文本与工具调用参数
//!   进入 LLM 请求前，命中密钥形态 → `<secret:N>` HMAC 短哈希占位符。同一明文在同一
//!   密钥下恒得同一占位符（确定性 HMAC），模型可稳定引用。
//! - **入向 restore**：模型输出（assistant 文本 + 工具调用参数）中的 `<secret:N>` 经
//!   会话级映射表还原为明文，保证后续工具调用拿到真实字节（编辑/命令精确匹配）。
//! - **per-install 密钥**：32 字节随机数持久化于 `<config_dir>/secrets.key`（0600），
//!   占位符跨会话稳定（持久 transcript 可一致还原）；`GYRE_SECRETS=off` 显式关闭
//!   （默认开）。密钥永不进入模型上下文——占位符哈希基于 HMAC，攻击者无密钥则无法
//!   对低熵候选做字典反推（omp `buildHashBase` 同理由）。
//!
//! 检测模式对照 omp 实际清单：
//! - 凭证形态主模式 `SENSITIVE_TOKEN_RE`（omp `packages/ai/src/providers/transform-messages.ts:419`
//!   内置唯一模式，含凭证熵门 + token 边界）：`gh[pousr]_` / `github_pat_` / `glpat-` /
//!   `sk-proj-` / `sk-ant-` / `sk-` 通用。
//! - 补充形态（omp `memories/index.ts:1130-1140` 与 `sharpshooter/consolidate.ts` 的通用
//!   `redactSecrets` 清单）：AWS `AKIA|ASIA`、Slack `xox*`、Google `AIza`、npm、JWT 三段式。
//! - 私钥 PEM 块、`Bearer/Basic` 头、通用高熵赋值（`api_key = "…"` 等）：任务点名补充，
//!   保守熵门控制误报（值长 ≥16 且至少两类字符）。

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::message::{AssistantMessage, ContentBlock, ProviderMessage, UserContent};

type HmacSha256 = Hmac<Sha256>;

// ═══════════════════════════════════════════════════════════════════════════
// 常量
// ═══════════════════════════════════════════════════════════════════════════

/// 占位符哈希的 hex 长度：HMAC-SHA256 输出截断 64 bit。与 omp `HASH_LEN = 12`（base36
/// ≈62 bit）同级抗碰撞：密钥数量级（单会话 ≤ 数千）下碰撞概率可忽略。
const PLACEHOLDER_HEX_LEN: usize = 16;

/// 占位符形态：`<secret:` + 16 hex + `>`。
const PLACEHOLDER_PREFIX: &str = "<secret:";
const PLACEHOLDER_SUFFIX: &str = ">";

/// 占位符扫描正则（restore 用）。与 [`PLACEHOLDER_HEX_LEN`] 必须同步。
static PLACEHOLDER_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    // 展开即 `<secret:([0-9a-f]{16})>`。
    regex::Regex::new(&format!(
        r"{}([0-9a-f]{{{}}}){}",
        regex::escape(PLACEHOLDER_PREFIX),
        PLACEHOLDER_HEX_LEN,
        regex::escape(PLACEHOLDER_SUFFIX)
    ))
    .expect("占位符正则为编译期常量，必然合法")
});

/// per-install 密钥文件名（相对 [`crate::platform::config_dir`]）。
const KEY_FILE_NAME: &str = "secrets.key";

/// per-install 密钥字节数（对齐 omp `crypto.randomBytes(32)`）。
const KEY_LEN: usize = 32;

/// 显式关闭开关的环境变量名。值 `off` / `0` / `false` / `no`（大小写不敏感）关闭，
/// 未设置或其它值开启（默认开）。
pub const ENV_OFF_VAR: &str = "GYRE_SECRETS";

// ═══════════════════════════════════════════════════════════════════════════
// 检测模式集
// ═══════════════════════════════════════════════════════════════════════════

/// 熵门类型（对齐 omp 的两级保守策略）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntropyGate {
    /// 无熵门（形态本身足够特异：定长前缀、定长大写等）。
    None,
    /// 凭证熵门：移植 omp `hasPlausibleCredentialEntropy`（pi-ai transform-messages.ts:421）——
    /// 剥离已知前缀后，剩余部分至少含 [小写/大写/数字/`_-`] 中两类（全 `*` 视为掩码直接命中）。
    Credential,
    /// 值熵门：至少含 1 位数字且至少含 1 个字母（通用赋值/请求头场景的保守过滤）。
    /// 仅要求「两类」不足以排除代码标识符——camelCase 如 `someVariableName` 恰有
    /// 小写+大写两类；真实密钥（≥16 长度）几乎必然含数字，以「含数字」作判别条件。
    ValueClasses,
}

/// 一条已编译的检测模式。
struct CompiledPattern {
    regex: regex::Regex,
    /// 命中后替换的捕获组索引（0 = 整体匹配；赋值类模式取值的捕获组）。
    group: usize,
    /// 是否要求 token 边界（对齐 omp `SENSITIVE_TOKEN_RE` 的 lookbehind/lookahead：
    /// 前后紧邻 `[a-zA-Z0-9_*-]` 时不算命中——避免把更长 token 的片段截出来）。
    boundary: bool,
    entropy: EntropyGate,
}

/// 全局模式集（`OnceLock` 编译一次；`SecretsObfuscator` 多实例共享，免重复编译）。
static PATTERNS: OnceLock<Vec<CompiledPattern>> = OnceLock::new();

/// omp `SENSITIVE_TOKEN_RE` 的 source（pi-ai transform-messages.ts:418-419 原样移植）。
/// lookaround 由 [`CompiledPattern::boundary`] 手工实现（Rust `regex` 不支持）。
const SENSITIVE_TOKEN_BODY: &str = concat!(
    r"(gh[opusr]_[a-zA-Z0-9_*]{36,}",
    r"|github_pat_[a-zA-Z0-9_*]{36,}",
    r"|glpat-[a-zA-Z0-9_*-]{20,}",
    r"|sk-proj-[a-zA-Z0-9_*-]{36,}",
    r"|sk-ant-[a-zA-Z0-9_*-]{36,}",
    r"|sk-[a-zA-Z0-9_*-]{48,})",
);

/// 构建模式集。顺序即优先级：特异形态在前，宽泛赋值在后；重叠命中保留先出现者。
fn build_patterns() -> Vec<CompiledPattern> {
    let p = |src: &str, group: usize, boundary: bool, entropy: EntropyGate| CompiledPattern {
        regex: regex::Regex::new(src).expect("secrets 模式为编译期常量，必然合法"),
        group,
        boundary,
        entropy,
    };
    vec![
        // 1. omp 内置凭证形态（GitHub/GitLab/OpenAI/Anthropic API key），含凭证熵门 + 边界。
        p(SENSITIVE_TOKEN_BODY, 1, true, EntropyGate::Credential),
        // 2-6. omp memories/sharpshooter 通用 redactSecrets 清单的补充形态。
        // AWS 访问密钥 ID（20 位大写数字，形态固定）。
        p(r"((?:AKIA|ASIA)[A-Z0-9]{16})", 1, true, EntropyGate::None),
        // Slack token（xoxb/xoxa/xoxp/xoxr/xoxs-）。
        p(
            r"(xox[baprs]-[A-Za-z0-9-]{10,})",
            1,
            true,
            EntropyGate::None,
        ),
        // Google API key。
        p(r"(AIza[A-Za-z0-9_-]{30,})", 1, true, EntropyGate::None),
        // npm token。
        p(r"(npm_[A-Za-z0-9]{30,})", 1, true, EntropyGate::None),
        // JWT 三段式（header.payload.signature；段长 ≥16 已排除版本号类误报）。
        p(
            r"([A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,})",
            1,
            true,
            EntropyGate::None,
        ),
        // 7. PEM 私钥块（整块替换；任务点名，omp 在日志/分享脱敏中处理同类形态）。
        p(
            r"(-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----[\s\S]*?-----END (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----)",
            1,
            false,
            EntropyGate::None,
        ),
        // 8. Bearer/Basic 请求头（omp mcp/errors.ts:90 的形态；仅替换 token 部分）。
        p(
            r#"(?i)\b(?:bearer|basic)\s+["']?([A-Za-z0-9_\-.=+/]{16,})["']?"#,
            1,
            false,
            EntropyGate::ValueClasses,
        ),
        // 9. 通用高熵赋值：密义键名 + `=`/`:` + 值长 ≥16 + 值熵门（含数字+字母；
        //    保守，控制误报）。键名允许任意词字符前缀（`OPENAI_API_KEY`、`my_token` 等
        //    `ENV_STYLE` 命名）；真实命中与否由值熵门裁决。
        p(
            r#"(?i)\b[\w.-]*(?:api[_-]?key|apikey|secret|token|password|passwd|pwd|credential|access[_-]?key|private[_-]?key|client[_-]?secret|auth[_-]?token)["']?\s*[:=]\s*["']?([A-Za-z0-9+/_\-.=]{16,})["']?"#,
            1,
            false,
            EntropyGate::ValueClasses,
        ),
    ]
}

/// 返回全局模式集。
fn patterns() -> &'static [CompiledPattern] {
    PATTERNS.get_or_init(build_patterns)
}

/// 凭证熵门（omp `hasPlausibleCredentialEntropy` 原样移植）。
fn has_plausible_credential_entropy(token: &str) -> bool {
    let lower = token.to_lowercase();
    let prefix_len = if lower.starts_with("github_pat_") {
        "github_pat_".len()
    } else if lower.starts_with("glpat-") {
        "glpat-".len()
    } else if lower.starts_with("sk-proj-") {
        "sk-proj-".len()
    } else if lower.starts_with("sk-ant-") {
        "sk-ant-".len()
    } else if lower.starts_with("gh") {
        4
    } else {
        3
    };
    // 模式保证 token 长度 ≥ 前缀长度，但防御性钳制。
    let secret = token.get(prefix_len.min(token.len())..).unwrap_or(token);
    // 全 `*` 视为已掩码 token（omp：`/^\*+$/` 直接命中）。
    if !secret.is_empty() && secret.chars().all(|c| c == '*') {
        return true;
    }
    let classes = [
        secret.chars().any(char::is_lowercase),
        secret.chars().any(char::is_uppercase),
        secret.chars().any(|c: char| c.is_ascii_digit()),
        secret.chars().any(|c| c == '_' || c == '-'),
    ];
    classes.iter().filter(|b| **b).count() >= 2
}

/// 值熵门：至少含 1 位数字且至少含 1 个字母（见 [`EntropyGate::ValueClasses`]：
/// 「含数字」排除 camelCase 等纯字母代码标识符，保守控制误报）。
fn has_plausible_value_entropy(value: &str) -> bool {
    let has_digit = value.chars().any(|c: char| c.is_ascii_digit());
    let has_letter = value.chars().any(char::is_alphabetic);
    has_digit && has_letter
}

/// token 边界检查（对齐 `SENSITIVE_TOKEN_RE` 的负向环视：前后紧邻
/// `[a-zA-Z0-9_*-]` 时拒绝——说明命中是更长 token 的内部片段）。
fn boundary_ok(text: &str, start: usize, end: usize) -> bool {
    let edge = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '*' || c == '-';
    let left_ok = text[..start].chars().next_back().is_none_or(|c| !edge(c));
    let right_ok = text[end..].chars().next().is_none_or(|c| !edge(c));
    left_ok && right_ok
}

// ═══════════════════════════════════════════════════════════════════════════
// SecretsObfuscator
// ═══════════════════════════════════════════════════════════════════════════

/// 一次扫描中的命中。
struct Hit {
    start: usize,
    end: usize,
    secret: String,
}

/// 密钥双向脱敏器（会话级：持有一张 `占位符 → 明文` 映射表，供入向还原）。
///
/// 实例放 [`crate::platform`] 侧由 host 构造、经 `AgentBuilder::secrets` 注入 engine；
/// 生命周期 = 会话（engine）。`Send + Sync`。
pub struct SecretsObfuscator {
    /// per-install HMAC 密钥（32B；`ephemeral` 构造时为进程内随机）。
    key: [u8; KEY_LEN],
    /// 会话级映射表：占位符（`<secret:hex16>` 的 hex 段）→ 明文。
    restore_map: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for SecretsObfuscator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 密钥与映射表内容不入 Debug（防止日志间接泄漏）。
        f.debug_struct("SecretsObfuscator")
            .field("key", &"<redacted>")
            .field("restore_map_len", &self.restore_map.lock().unwrap().len())
            .finish()
    }
}

impl SecretsObfuscator {
    /// 从给定 HMAC 密钥构造（测试 / 显式注入）。
    #[must_use]
    pub fn with_key(key: [u8; KEY_LEN]) -> Self {
        Self {
            key,
            restore_map: Mutex::new(HashMap::new()),
        }
    }

    /// 进程内随机密钥构造（config 目录不可用时的降级：脱敏仍开，占位符仅本进程内稳定）。
    #[must_use]
    pub fn ephemeral() -> Self {
        Self::with_key(random_key())
    }

    /// 读取 env 开关：`GYRE_SECRETS` 为 `off`/`0`/`false`/`no`（大小写不敏感）时关闭。
    /// 未设置或其它值 → 开启（默认开）。
    #[must_use]
    pub fn env_enabled() -> bool {
        parse_enabled(std::env::var(ENV_OFF_VAR).ok().as_deref())
    }

    /// 从 per-install 密钥文件构造：`<config_dir>/secrets.key`（0600）。
    ///
    /// - env 关闭 → `None`（`GYRE_SECRETS=off` 显式关闭，默认开）。
    /// - config 目录未知 → 降级 [`Self::ephemeral`]（脱敏仍开）。
    /// - 文件存在且为 32B → 复用（跨会话占位符稳定）；缺失 → 生成并落盘（0600）；
    ///   损坏/长度不符 → 警告后重新生成覆盖（旧占位符跨版本不再可还原，可接受）。
    /// - 落盘失败（只读 fs 等）→ 降级 [`Self::ephemeral`] 并警告。
    #[must_use]
    pub fn load(config_dir: Option<&Path>) -> Option<Self> {
        if !Self::env_enabled() {
            return None;
        }
        let Some(dir) = config_dir else {
            return Some(Self::ephemeral());
        };
        Some(Self::with_key_file(&key_path(dir)))
    }

    /// 默认装配入口（host 接线用）：[`Self::load`] + [`crate::platform::config_dir`]。
    #[must_use]
    pub fn load_default() -> Option<Self> {
        Self::load(crate::platform::config_dir().as_deref())
    }

    /// 密钥文件加载 / 生成（见 [`Self::load`] 文档）。
    #[must_use]
    pub fn with_key_file(path: &Path) -> Self {
        // 读现有：合法 32B 即复用。
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(key) = <[u8; KEY_LEN]>::try_from(bytes.as_slice()) {
                // 复用时顺带加固权限（历史文件可能宽松）。
                harden_permissions(path);
                return Self::with_key(key);
            }
            tracing::warn!(
                path = %path.display(),
                len = bytes.len(),
                "secrets.key 损坏（长度非 32B），重新生成；旧占位符不再可还原"
            );
        }
        let key = random_key();
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(error = %e, dir = %parent.display(), "secrets.key 目录创建失败，降级进程内密钥");
                return Self::with_key(key);
            }
        }
        // 0600 写入（unix）；写失败降级。
        let write = std::fs::File::create(path).and_then(|f| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{FileExt, PermissionsExt};
                f.write_all_at(&key, 0)?;
                drop(f);
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            #[cfg(not(unix))]
            {
                use std::io::Write;
                let mut f = f;
                f.write_all(&key)?;
            }
            Ok(())
        });
        match write {
            Ok(()) => {
                tracing::info!(path = %path.display(), "已生成 per-install 密钥文件 secrets.key（0600）");
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "secrets.key 写入失败，降级进程内密钥");
            }
        }
        Self::with_key(key)
    }

    // ── 出向 transform ─────────────────────────────────────────────────────

    /// 扫描文本，命中密钥 → `<secret:N>` 占位符并登记映射表。
    /// 无命中时零拷贝返回借用（Cow::Borrowed）。
    #[must_use]
    pub fn obfuscate<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let hits = collect_hits(text);
        if hits.is_empty() {
            return Cow::Borrowed(text);
        }
        let mut map = self.restore_map.lock().unwrap();
        let mut out = String::with_capacity(text.len());
        let mut pos = 0;
        for h in &hits {
            out.push_str(&text[pos..h.start]);
            out.push_str(&self.placeholder_locked(&mut map, &h.secret));
            pos = h.end;
        }
        out.push_str(&text[pos..]);
        Cow::Owned(out)
    }

    /// 递归走查 JSON（工具调用参数是任意模型生成 JSON，唯一需要递归的面；
    /// 对齐 omp `deobfuscateToolArguments` 的 JSON walk 例外）。
    pub fn obfuscate_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(s) => {
                if let Cow::Owned(o) = self.obfuscate(s) {
                    *s = o;
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    self.obfuscate_json(item);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values_mut() {
                    self.obfuscate_json(item);
                }
            }
            _ => {}
        }
    }

    /// Provider 线消息批量出向脱敏（engine 在构造 `CompletionRequest` 前调用）。
    ///
    /// 覆盖面（对齐 omp `obfuscateMessages`，message-transform.ts:248）：
    /// - `User` 文本块、`Tool` 结果文本：明文 → 占位符；
    /// - `Assistant` 文本与工具参数：本地存储为明文，重放给 provider 前再脱敏
    ///   （omp `obfuscateAssistantContentForReplay` 同理）；
    /// - `Thinking` 跳过：思考块携带 provider 签名，字节级绑定（改动会破坏回放校验），
    ///   且 thinking 不参与工具执行；
    /// - `System` 跳过：静态系统提示词不承载会话密钥（omp 同）。
    #[must_use]
    pub fn obfuscate_provider_messages(
        &self,
        messages: Vec<ProviderMessage>,
    ) -> Vec<ProviderMessage> {
        messages
            .into_iter()
            .map(|m| self.obfuscate_provider_message(m))
            .collect()
    }

    /// 单条 Provider 线消息出向脱敏（见 [`Self::obfuscate_provider_messages`] 覆盖面说明）。
    #[must_use]
    pub fn obfuscate_provider_message(&self, message: ProviderMessage) -> ProviderMessage {
        match message {
            ProviderMessage::User { mut content } => {
                for block in &mut content {
                    if let UserContent::Text { text } = block {
                        *text = self.obfuscate(text).into_owned();
                    }
                }
                ProviderMessage::User { content }
            }
            ProviderMessage::Tool {
                tool_call_id,
                content,
                is_error,
                images,
            } => ProviderMessage::Tool {
                tool_call_id,
                content: self.obfuscate(&content).into_owned(),
                is_error,
                images,
            },
            ProviderMessage::Assistant { mut content } => {
                for block in &mut content {
                    match block {
                        ContentBlock::Text { text } => *text = self.obfuscate(text).into_owned(),
                        ContentBlock::ToolCall { arguments, .. } => self.obfuscate_json(arguments),
                        ContentBlock::Thinking { .. } => {}
                    }
                }
                ProviderMessage::Assistant { content }
            }
            ProviderMessage::System(_) => message,
        }
    }

    // ── 入向 restore ───────────────────────────────────────────────────────

    /// 还原文本中的 `<secret:N>` 占位符（查会话级映射表）。
    /// 表外占位符（其它密钥/会话产物）保持原样，绝不猜。
    #[must_use]
    pub fn restore<'a>(&self, text: &'a str) -> Cow<'a, str> {
        // 快速路径：无占位符前缀则零拷贝。
        if !text.contains(PLACEHOLDER_PREFIX) {
            return Cow::Borrowed(text);
        }
        let map = self.restore_map.lock().unwrap();
        PLACEHOLDER_RE.replace_all(text, |caps: &regex::Captures| {
            map.get(&caps[1])
                .cloned()
                .unwrap_or_else(|| caps[0].to_string())
        })
    }

    /// 原地还原（engine 对累计流式文本 / 消息块使用）。
    pub fn restore_str(&self, text: &mut String) {
        if let Cow::Owned(o) = self.restore(text) {
            *text = o;
        }
    }

    /// 递归还原 JSON 中的占位符（工具调用参数）。
    pub fn restore_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(s) => self.restore_str(s),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.restore_json(item);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values_mut() {
                    self.restore_json(item);
                }
            }
            _ => {}
        }
    }

    /// 还原整条 assistant 消息（engine 在消息最终化后、入 context / `MessageEnd` /
    /// 工具分发前调用）：文本块与工具参数还原，`Thinking` 跳过（同出向理由）。
    ///
    /// restore 先于一切下游消费 = 单一真相源：持久化、UI 快照、`tool_calls` 提取、
    /// 工具执行全部看到明文；下一轮重放时出向 transform 再统一脱敏（闭环）。
    pub fn restore_assistant_message(&self, message: &mut AssistantMessage) {
        for block in &mut message.content {
            match block {
                ContentBlock::Text { text } => self.restore_str(text),
                ContentBlock::ToolCall { arguments, .. } => self.restore_json(arguments),
                ContentBlock::Thinking { .. } => {}
            }
        }
    }

    // ── 占位符 ─────────────────────────────────────────────────────────────

    /// 生成占位符并登记映射表（调用方须持锁）。
    fn placeholder_locked(&self, map: &mut HashMap<String, String>, secret: &str) -> String {
        let tag = hmac_short_hex(&self.key, secret);
        map.entry(tag.clone()).or_insert_with(|| secret.to_string());
        format!("{PLACEHOLDER_PREFIX}{tag}{PLACEHOLDER_SUFFIX}")
    }

    /// 计算某明文的占位符（不登记映射表；测试与观测用）。
    #[must_use]
    pub fn placeholder_for(&self, secret: &str) -> String {
        let tag = hmac_short_hex(&self.key, secret);
        format!("{PLACEHOLDER_PREFIX}{tag}{PLACEHOLDER_SUFFIX}")
    }

    /// 映射表登记的密钥条数（观测用）。
    #[must_use]
    pub fn mapped_secrets(&self) -> usize {
        self.restore_map.lock().unwrap().len()
    }
}

/// 密钥文件绝对路径：`<config_dir>/secrets.key`。
#[must_use]
pub fn key_path(config_dir: &Path) -> PathBuf {
    config_dir.join(KEY_FILE_NAME)
}

/// env 值解析（纯函数，供单测穷举）：`off`/`0`/`false`/`no`（大小写不敏感）→ `false`。
#[must_use]
pub fn parse_enabled(env: Option<&str>) -> bool {
    match env {
        None => true,
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        ),
    }
}

/// HMAC-SHA256(key, value) 截断 [`PLACEHOLDER_HEX_LEN`] 位 hex。
fn hmac_short_hex(key: &[u8; KEY_LEN], value: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(value.as_bytes());
    let out = mac.finalize().into_bytes();
    let mut s = String::with_capacity(PLACEHOLDER_HEX_LEN);
    for b in out.iter().take(PLACEHOLDER_HEX_LEN / 2) {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// OS 熵源生成 32B 密钥。
fn random_key() -> [u8; KEY_LEN] {
    use rand::RngCore;
    let mut key = [0u8; KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

/// 收紧密钥文件权限到 0600（unix；尽力而为，失败仅警告）。
fn harden_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o600 {
                if let Err(e) =
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                {
                    tracing::warn!(error = %e, path = %path.display(), "secrets.key 权限收紧失败");
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 扫描全部模式，收集经边界 / 熵门过滤、去重叠后的命中（按 start 升序）。
fn collect_hits(text: &str) -> Vec<Hit> {
    let mut all: Vec<Hit> = Vec::new();
    for p in patterns() {
        for caps in p.regex.captures_iter(text) {
            let Some(m) = caps.get(p.group) else { continue };
            let (start, end) = (m.start(), m.end());
            if start == end {
                continue;
            }
            if p.boundary && !boundary_ok(text, start, end) {
                continue;
            }
            let secret = m.as_str();
            let pass = match p.entropy {
                EntropyGate::None => true,
                EntropyGate::Credential => has_plausible_credential_entropy(secret),
                EntropyGate::ValueClasses => has_plausible_value_entropy(secret),
            };
            if !pass {
                continue;
            }
            all.push(Hit {
                start,
                end,
                secret: secret.to_string(),
            });
        }
    }
    // 排序 + 去重叠：优先保留更早模式（更特异）与更长命中。
    all.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut kept: Vec<Hit> = Vec::with_capacity(all.len());
    let mut last_end = 0;
    for h in all {
        if h.start >= last_end {
            last_end = h.end;
            kept.push(h);
        }
    }
    kept
}

// ═══════════════════════════════════════════════════════════════════════════
// 单测
// ═══════════════════════════════════════════════════════════════════════════

/// 正则凭据掩码器（不可逆；H45：DAP/LSP 之外的第二套「脱敏」实现收敛到 core）。
///
/// 与 [`SecretsObfuscator`] 的区别：后者是**可逆**的 HMAC 占位符（供 provider 消息往返
/// 还原），本类型是**不可逆**的模式掩码（把命中片段替换为 `<redacted>`），用于把快照/
/// 上下文喂给外部模型前的最后一道脱敏。
///
/// 语义移植 oh-my-pi `SecretObfuscator`：内置高频 pattern（API key / token / AWS /
/// GitHub PAT / Bearer / 常见 env 导出），并支持调用方追加 pattern。
pub struct PatternRedactor {
    patterns: Vec<regex::Regex>,
}

impl Default for PatternRedactor {
    fn default() -> Self {
        Self::new()
    }
}

/// 内置脱敏 pattern（顺序即应用顺序）。
const BUILTIN_REDACT_PATTERNS: &[&str] = &[
    // 形如 `key = value` / `key: value` / `key=value`（引号可选，值 ≥ 8 字符）。
    r#"(?i)(api[_-]?key|access[_-]?key|secret|password|passwd|token)\s*[:=]\s*["']?[A-Za-z0-9_\-./+]{8,}"#,
    // OpenAI / Anthropic 风格。
    r"sk-[A-Za-z0-9_\-]{16,}",
    r"sk-ant-[A-Za-z0-9_\-]{16,}",
    // GitHub。
    r"ghp_[A-Za-z0-9]{20,}",
    r"github_pat_[A-Za-z0-9_]{20,}",
    // AWS access key。
    r"AKIA[0-9A-Z]{16}",
    // Bearer 头。
    r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{12,}",
    // 常见环境变量导出。
    r#"(?i)(export\s+)?(OPENAI|ANTHROPIC|GEMINI|GITHUB|HUGGINGFACE|GOOGLE|AZURE)_?(API|ACCESS|SECRET|TOKEN|KEY)["']?=["']?[A-Za-z0-9_\-./+]{8,}"#,
];

impl PatternRedactor {
    /// 内置 pattern 构造器。
    ///
    /// # Panics
    /// 内置 pattern 为编译期常量，非法即编程错误（与仓库其它 `expect` 一致）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            patterns: BUILTIN_REDACT_PATTERNS
                .iter()
                .map(|p| regex::Regex::new(p).expect("内置脱敏 pattern 必须合法"))
                .collect(),
        }
    }

    /// 追加自定义 pattern（用户/仓库级）。
    pub fn add_pattern(&mut self, re: regex::Regex) {
        self.patterns.push(re);
    }

    /// 掩码文本中的凭据（命中片段 → `<redacted>`）。
    #[must_use]
    pub fn obfuscate(&self, text: &str) -> String {
        let mut out = text.to_string();
        for p in &self.patterns {
            out = p.replace_all(&out, "<redacted>").into_owned();
        }
        out
    }
}

// ── H45：PatternRedactor（正则脱敏单源）──

#[test]
fn pattern_redactor_masks_builtin_and_custom_patterns() {
    let r = PatternRedactor::new();
    // 键值形态 / 前缀形态 / Bearer / env 导出。
    for sample in [
        "api_key = sk-abcdefghijklmnopqrstuvwx",
        "token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        "Authorization: Bearer abcdefghijklmnop",
        "export OPENAI_API_KEY=abcdefghijklmnop",
        "aws AKIA1234567890ABCDEF",
    ] {
        let out = r.obfuscate(sample);
        assert!(out.contains("<redacted>"), "{sample} 应被掩码，实得 {out}");
    }
    // 正常文本不被误伤。
    assert_eq!(
        r.obfuscate("let x = 1; // 普通代码"),
        "let x = 1; // 普通代码"
    );
    // 自定义 pattern 追加生效（与 advisor 原实现同语义）。
    let mut r = PatternRedactor::new();
    r.add_pattern(regex::Regex::new(r"internal-[0-9]{6}").unwrap());
    assert!(r.obfuscate("internal-123456").contains("<redacted>"));
    // 幂等：重复掩码不再变化。
    let once = r.obfuscate("internal-123456 and sk-abcdefghijklmnopqrstuvwx");
    assert_eq!(once, r.obfuscate(&once));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, StopReason, Usage};

    fn obf() -> SecretsObfuscator {
        SecretsObfuscator::with_key([7u8; KEY_LEN])
    }
    /// 真实形态 GitHub token（前缀 4 + 体 36，对齐 omp `{36,}`）；检测类测试共用。
    const GHP: &str = "ghp_Ab12Cd34Ef56Gh78Ij90Kl12Mn34Op56Qr78";

    // ── 各模式检测 ─────────────────────────────────────────────────────────

    #[test]
    fn 检测_omp_内置凭证形态_全前缀() {
        let o = obf();
        // gh*_ / github_pat_ 体长按 omp `{36,}`（真实 GitHub token 形态）。
        for (name, token) in [
            ("ghp_", "ghp_Ab12Cd34Ef56Gh78Ij90Kl12Mn34Op56Qr78"),
            ("gho_", "gho_Ab12Cd34Ef56Gh78Ij90Kl12Mn34Op56Qr78"),
            (
                "github_pat_",
                "github_pat_Ab12Cd34Ef56Gh78Ij90Kl12Mn34Op56Qr78",
            ),
            ("glpat-", "glpat-Ab12Cd34Ef56Gh78Ij90Kl12"),
            ("sk-proj-", "sk-proj-Ab12Cd34Ef56Gh78Ij90Kl12Mn34Op56Qr78"),
            ("sk-ant-", "sk-ant-Ab12Cd34Ef56Gh78Ij90Kl12Mn34Op56Qr78"),
            (
                "sk-通用",
                "sk-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0U1v2W3x4",
            ),
        ] {
            let input = format!("key is {token} end");
            let out = o.obfuscate(&input);
            assert!(out.contains("<secret:"), "{name} 应命中，got: {out}");
            assert!(!out.contains(token), "{name} 明文应被替换");
        }
    }

    #[test]
    fn 凭证熵门_低熵跳过() {
        let o = obf();
        // 全同字符重复、单类字符 → 熵门拒绝（omp：至少两类）。
        let out = o.obfuscate("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(!out.contains("<secret:"), "低熵 sk- 应跳过，got: {out}");
    }

    #[test]
    fn 凭证边界_更长token内部片段不命中() {
        let o = obf();
        // 长度合法（否则测的是长度门而非边界门）：紧邻字母 → lookbehind 等价拒绝。
        let input = format!("X{GHP}");
        let out = o.obfuscate(&input);
        assert!(!out.contains("<secret:"), "内部片段不应命中，got: {out}");
    }

    #[test]
    fn 检测_补充形态_aws_slack_google_npm_jwt() {
        let o = obf();
        for token in [
            "AKIAIOSFODNN7EXAMPLE",
            "ASIAIOSFODNN7EXAMPLE",
            "xoxb-123456789012-AbCdEfGhIjKl",
            "AIzaSyA1234567890abcdefghijklmnopqrstuv",
            "npm_AbCdEfGhIjKlMnOpQrStUvWx1234567890",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9P",
        ] {
            let input = format!("value={token};");
            let out = o.obfuscate(&input);
            assert!(out.contains("<secret:"), "{token} 应命中，got: {out}");
        }
    }

    #[test]
    fn 检测_pem私钥块() {
        let o = obf();
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";
        let input = format!("cert:\n{pem}\ntrailing");
        let out = o.obfuscate(&input);
        assert!(out.contains("<secret:"), "PEM 块应整体命中，got: {out}");
        assert!(!out.contains("BEGIN"), "块内容不应残留");
    }

    #[test]
    fn 检测_bearer头与通用赋值() {
        let o = obf();
        let out = o.obfuscate("Authorization: Bearer AbCdEf123456GhIjKlMnOp");
        assert!(out.contains("<secret:"), "bearer 值应命中，got: {out}");
        assert!(
            out.contains("Bearer <secret:"),
            "只替换 token，保留 Bearer 前缀，got: {out}"
        );

        let out = o.obfuscate("OPENAI_API_KEY = \"sk-style-A1b2C3d4E5f6G7h8\"");
        assert!(out.contains("<secret:"), "赋值形态应命中，got: {out}");

        let out = o.obfuscate("api_key: 4f8a9b2c1d3e5f6a");
        assert!(out.contains("<secret:"), "yaml 冒号赋值应命中，got: {out}");
    }

    #[test]
    fn 通用赋值_低熵与短值跳过() {
        let o = obf();
        // 值 <16 或单类（纯小写单词）→ 跳过。
        let out = o.obfuscate("token: someVariableName color: red timeout: 30");
        assert!(!out.contains("<secret:"), "低熵/短值不应命中，got: {out}");
    }

    // ── placeholder 稳定性 ─────────────────────────────────────────────────

    #[test]
    fn placeholder_同明文同占位_异明文异占位() {
        let o = obf();
        let p1 = o.placeholder_for("ghp_Ab12Cd34Ef56Gh78Ij90Kl12Mn34");
        let p2 = o.placeholder_for("ghp_Ab12Cd34Ef56Gh78Ij90Kl12Mn34");
        let p3 = o.placeholder_for("ghp_Zz99Yy88Xx77Ww66Vv55Uu44Tt33");
        assert_eq!(p1, p2, "同明文（同 key）必须同占位符");
        assert_ne!(p1, p3, "不同明文必须不同占位符");
        assert_eq!(
            p1.len(),
            PLACEHOLDER_PREFIX.len() + PLACEHOLDER_HEX_LEN + PLACEHOLDER_SUFFIX.len()
        );
        // 形态校验：16 hex。
        let tag = p1
            .trim_start_matches(PLACEHOLDER_PREFIX)
            .trim_end_matches(PLACEHOLDER_SUFFIX);
        assert!(tag.len() == PLACEHOLDER_HEX_LEN && tag.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn placeholder_不同key不同占位() {
        let a = SecretsObfuscator::with_key([1u8; KEY_LEN]);
        let b = SecretsObfuscator::with_key([2u8; KEY_LEN]);
        assert_ne!(a.placeholder_for("secret"), b.placeholder_for("secret"));
    }

    // ── roundtrip ──────────────────────────────────────────────────────────

    #[test]
    fn roundtrip_出向脱敏_入向还原() {
        let o = obf();
        let secret = GHP;
        let input = format!("token is {secret} thanks");
        let out = o.obfuscate(&input);
        assert!(out.contains("<secret:"));
        let back = o.restore(&out);
        assert_eq!(
            back,
            format!("token is {secret} thanks"),
            "restore 必须无损还原"
        );
    }

    #[test]
    fn roundtrip_工具参数_json_walk() {
        let o = obf();
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let mut args: serde_json::Value = serde_json::json!({
            "command": "aws sts get-caller-identity",
            "env": { "AWS_ACCESS_KEY_ID": secret, "nested": ["x", { "deep": secret }] },
            "n": 42,
        });
        o.obfuscate_json(&mut args);
        let s = args.to_string();
        assert!(!s.contains(secret), "JSON 内明文应全部替换");
        o.restore_json(&mut args);
        assert_eq!(args["env"]["AWS_ACCESS_KEY_ID"], secret);
        assert_eq!(args["env"]["nested"][1]["deep"], secret);
    }

    #[test]
    fn restore_assistant_message_文本与工具参数均还原_思考跳过() {
        let o = obf();
        let secret = GHP;
        // placeholder_for 不登记映射表；先经 obfuscate 注册（占位符确定性，二者一致）。
        let ph = o.obfuscate(secret).into_owned();
        let mut msg = AssistantMessage {
            content: vec![
                ContentBlock::Text {
                    text: format!("found {ph} in env"),
                },
                ContentBlock::Thinking {
                    text: format!("saw {ph}"),
                    signature: Some("sig".into()),
                },
                ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "run_command".into(),
                    arguments: serde_json::json!({ "command": format!("echo {ph}") }),
                    signature: None,
                },
            ],
            usage: Usage::default(),
            model: "m".into(),
            stop_reason: Some(StopReason::Stop),
            stop_details: None,
        };
        o.restore_assistant_message(&mut msg);
        assert!(
            msg.content[0].as_text().unwrap().contains(secret),
            "文本应还原"
        );
        let ContentBlock::Thinking { text, .. } = &msg.content[1] else {
            panic!()
        };
        assert!(text.contains(&ph), "Thinking 块跳过（字节级回放）");
        let ContentBlock::ToolCall { arguments, .. } = &msg.content[2] else {
            panic!()
        };
        assert_eq!(
            arguments["command"],
            format!("echo {secret}"),
            "工具参数应还原"
        );
    }

    #[test]
    fn restore_表外占位符保持原样() {
        let o = obf();
        let foreign = "<secret:0123456789abcdef>";
        assert_eq!(
            o.restore(&format!("x {foreign} y")),
            format!("x {foreign} y")
        );
    }

    #[test]
    fn provider_messages_出向覆盖_user_tool_assistant_跳过thinking与system() {
        let o = obf();
        let secret = GHP;
        let msgs = vec![
            ProviderMessage::System("static prompt".into()),
            ProviderMessage::User {
                content: vec![UserContent::Text {
                    text: format!("leak {secret}"),
                }],
            },
            ProviderMessage::Tool {
                tool_call_id: "t1".into(),
                content: format!("output {secret}"),
                is_error: false,
                images: Vec::new(),
            },
            ProviderMessage::Assistant {
                content: vec![
                    ContentBlock::Text {
                        text: format!("replay {secret}"),
                    },
                    ContentBlock::Thinking {
                        text: format!("think {secret}"),
                        signature: None,
                    },
                ],
            },
        ];
        let out = o.obfuscate_provider_messages(msgs);
        let assert_no_secret =
            |s: &str, what: &str| assert!(!s.contains(secret), "{what} 不应含明文");
        let ProviderMessage::System(s) = &out[0] else {
            panic!()
        };
        assert_eq!(s, "static prompt", "system 不动");
        let ProviderMessage::User { content } = &out[1] else {
            panic!()
        };
        let UserContent::Text { text } = &content[0] else {
            panic!()
        };
        assert!(text.contains("<secret:"));
        assert_no_secret(text, "user");
        let ProviderMessage::Tool { content, .. } = &out[2] else {
            panic!()
        };
        assert!(content.contains("<secret:"));
        assert_no_secret(content, "tool");
        let ProviderMessage::Assistant { content } = &out[3] else {
            panic!()
        };
        let ContentBlock::Text { text } = &content[0] else {
            panic!()
        };
        assert!(text.contains("<secret:"), "assistant 重放文本应再脱敏");
        let ContentBlock::Thinking { text, .. } = &content[1] else {
            panic!()
        };
        assert!(text.contains(secret), "thinking 跳过");
    }

    #[test]
    fn obfuscate_无命中零拷贝() {
        let o = obf();
        let text = "普通文本 no secrets here";
        assert!(matches!(o.obfuscate(text), Cow::Borrowed(_)));
        assert!(matches!(o.restore(text), Cow::Borrowed(_)));
    }

    // ── off 开关 / key 文件 ────────────────────────────────────────────────

    #[test]
    fn env解析_纯函数穷举() {
        assert!(parse_enabled(None), "未设置默认开");
        assert!(!parse_enabled(Some("off")));
        assert!(!parse_enabled(Some("OFF")));
        assert!(!parse_enabled(Some(" 0 ")));
        assert!(!parse_enabled(Some("false")));
        assert!(!parse_enabled(Some("No")));
        assert!(parse_enabled(Some("on")));
        assert!(parse_enabled(Some("")));
        assert!(parse_enabled(Some("1")));
    }

    #[test]
    fn key文件_生成_权限_复用() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_path(dir.path());
        let a = SecretsObfuscator::with_key_file(&path);
        // 落盘 + 0600（unix）。
        let bytes = std::fs::read(&path).expect("key 文件应存在");
        assert_eq!(bytes.len(), KEY_LEN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "密钥文件必须 0600");
        }
        // 复用：新实例读同一文件 → 同 key → 同占位符。
        let b = SecretsObfuscator::with_key_file(&path);
        assert_eq!(
            a.placeholder_for("ghp_Ab12Cd34Ef56Gh78Ij90Kl12Mn34"),
            b.placeholder_for("ghp_Ab12Cd34Ef56Gh78Ij90Kl12Mn34")
        );
        // 损坏文件 → 重新生成（仍是合法 32B）。
        std::fs::write(&path, b"short").unwrap();
        let c = SecretsObfuscator::with_key_file(&path);
        assert_eq!(
            std::fs::read(&path).unwrap().len(),
            KEY_LEN,
            "损坏后应重写为合法长度"
        );
        let _ = c;
    }

    #[test]
    fn load_config缺目录降级_ephemeral() {
        // config_dir = None 且 env 未关闭 → ephemeral（进程内可用）。
        // 注：不在此测 env 关闭分支（改进程 env 与并行测试竞态），判定逻辑由
        // parse_enabled 穷举覆盖，load 分支组合为薄包装。
        let o = SecretsObfuscator::load(None);
        let o = o.expect("checked");
        let _ = o.obfuscate(GHP);
        assert!(o.mapped_secrets() >= 1);
    }
}
