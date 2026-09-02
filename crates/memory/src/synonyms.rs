//! P1 检索质量：同义词归一化（直译 mnemopi `core/synonyms.ts`）。
//!
//! 语义等价的词共享一个 canonical 词项：查询与内容两侧的 token 都映射到
//! canonical 后做 BM25 匹配，`db` 查询即命中 `database` 文档。STOP_WORDS
//! 在本实现中**不参与过滤**（过滤会改变既有 BM25 行为），仅保留表以对齐
//! 上游结构与后续扩展。

/// 同义词组：canonical 词 → 组内同义词（来自 mnemopi，43 组）。
pub const SYNONYM_GROUPS: &[(&str, &[&str])] = &[
    ("database", &["db", "datastore", "data_store"]),
    (
        "password",
        &["pass", "pwd", "passwd", "credential", "secret", "token"],
    ),
    ("config", &["configuration", "settings", "cfg", "setup"]),
    (
        "error",
        &[
            "bug",
            "issue",
            "fault",
            "failure",
            "crash",
            "exception",
            "traceback",
        ],
    ),
    (
        "fix",
        &["repair", "resolve", "solve", "patch", "correct", "address"],
    ),
    (
        "deploy",
        &["deployment", "release", "ship", "push", "rollout"],
    ),
    (
        "server",
        &["host", "machine", "vm", "instance", "node", "vps"],
    ),
    ("api", &["endpoint", "interface", "service"]),
    ("key", &["token", "credential", "secret", "api_key"]),
    ("user", &["account", "profile", "identity", "person"]),
    (
        "model",
        &["llm", "ai", "provider", "gpt", "claude", "gemini"],
    ),
    (
        "speed",
        &["fast", "quick", "performance", "latency", "throughput"],
    ),
    ("memory", &["recall", "remember", "storage", "retention"]),
    ("search", &["find", "lookup", "query", "retrieve", "locate"]),
    ("file", &["document", "doc", "text", "note"]),
    ("code", &["script", "program", "source", "implementation"]),
    ("test", &["verify", "check", "validate", "probe", "examine"]),
    ("backup", &["snapshot", "copy", "save", "archive"]),
    ("install", &["setup", "configure", "bootstrap", "init"]),
    ("update", &["upgrade", "refresh", "renew", "sync"]),
    (
        "delete",
        &["remove", "destroy", "purge", "clean", "wipe", "erase"],
    ),
    ("list", &["show", "display", "enumerate", "catalog"]),
    ("time", &["date", "when", "timestamp", "schedule"]),
    ("url", &["link", "address", "uri", "path"]),
    ("health", &["status", "check", "pulse", "alive", "up"]),
    ("service", &["daemon", "process", "systemd", "worker"]),
    ("port", &["socket", "bind", "listen"]),
    (
        "network",
        &["internet", "connection", "connectivity", "dns"],
    ),
    ("ssh", &["terminal", "shell", "remote", "connect"]),
    (
        "git",
        &["commit", "push", "pull", "repo", "repository", "branch"],
    ),
    ("log", &["output", "stdout", "stderr", "trace", "debug"]),
    ("cron", &["schedule", "job", "task", "timer", "periodic"]),
    ("email", &["mail", "message", "inbox", "smtp"]),
    ("image", &["picture", "photo", "screenshot", "graphic"]),
    ("browser", &["web", "page", "site", "navigate", "chrome"]),
    ("monitor", &["watch", "observe", "track", "survey"]),
    ("alert", &["notify", "notification", "warning", "ping"]),
    ("migrate", &["transfer", "move", "relocate", "port"]),
    ("compare", &["diff", "versus", "vs", "contrast"]),
    ("save", &["store", "persist", "preserve", "keep"]),
];

/// 停用词表（对齐 mnemopi；当前不参与过滤，保留供后续使用）。
#[allow(dead_code)]
pub const STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "have",
    "has",
    "had",
    "do",
    "does",
    "did",
    "will",
    "would",
    "could",
    "should",
    "may",
    "might",
    "can",
    "shall",
    "must",
    "i",
    "you",
    "he",
    "she",
    "it",
    "we",
    "they",
    "me",
    "him",
    "her",
    "us",
    "them",
    "my",
    "your",
    "his",
    "its",
    "our",
    "their",
    "mine",
    "yours",
    "hers",
    "ours",
    "theirs",
    "what",
    "which",
    "who",
    "whom",
    "where",
    "when",
    "why",
    "how",
    "this",
    "that",
    "these",
    "those",
    "of",
    "in",
    "to",
    "for",
    "on",
    "with",
    "at",
    "by",
    "from",
    "as",
    "into",
    "through",
    "during",
    "before",
    "after",
    "above",
    "below",
    "between",
    "under",
    "and",
    "but",
    "or",
    "nor",
    "not",
    "so",
    "than",
    "too",
    "very",
    "just",
    "about",
    "also",
    "really",
    "actually",
    "basically",
    "simply",
    "if",
    "then",
    "else",
    "while",
    "because",
    "though",
    "although",
];

/// 词 → canonical 的反向映射（canonical 自身映射到自身）。
static WORD_TO_CANONICAL: std::sync::LazyLock<
    std::collections::HashMap<&'static str, &'static str>,
> = std::sync::LazyLock::new(|| {
    let mut map = std::collections::HashMap::new();
    for &(canonical, synonyms) in SYNONYM_GROUPS {
        map.insert(canonical, canonical);
        for synonym in synonyms {
            map.insert(synonym, canonical);
        }
    }
    map
});

/// 单个词映射到 canonical；不在表中则原样返回。
#[must_use]
pub fn canonical_of(word: &str) -> &str {
    WORD_TO_CANONICAL.get(word).copied().unwrap_or(word)
}

/// 一组 token 映射到 canonical 并去重（保持首次出现顺序）。
/// 查询与内容两侧都调用，BM25 词项匹配即覆盖同义词。
#[must_use]
pub fn canonicalize_tokens(tokens: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        let canonical = canonical_of(&token);
        if seen.insert(canonical.to_string()) {
            out.push(canonical.to_string());
        }
    }
    out
}

/// 查询扩展（对齐 mnemopi `getSynonyms`）：返回词及其同义词全体。
/// 用于调试与展示；检索路径走 [`canonicalize_tokens`]。
#[must_use]
pub fn get_synonyms(word: &str) -> Vec<String> {
    let lowered = word.to_ascii_lowercase();
    let Some(canonical) = WORD_TO_CANONICAL.get(lowered.as_str()).copied() else {
        return vec![lowered];
    };
    let mut out = vec![canonical.to_string()];
    for (c, synonyms) in SYNONYM_GROUPS {
        if *c == canonical {
            out.extend(synonyms.iter().map(ToString::to_string));
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_maps_synonyms_to_group_head() {
        assert_eq!(canonical_of("db"), "database");
        assert_eq!(canonical_of("database"), "database");
        assert_eq!(canonical_of("passwd"), "password");
        assert_eq!(canonical_of("端到端"), "端到端"); // 中文原样
    }

    #[test]
    fn canonicalize_dedups_within_group() {
        // "fix" 与 "patch" 归一为同一 canonical → 去重后只留一项
        let tokens = vec!["fix".into(), "the".into(), "patch".into()];
        let out = canonicalize_tokens(tokens);
        assert_eq!(out, vec!["fix", "the"]);
    }

    #[test]
    fn get_synonyms_expands_group() {
        let syns = get_synonyms("db");
        assert_eq!(syns[0], "database");
        assert!(syns.contains(&"db".to_string()));
        assert!(syns.contains(&"datastore".to_string()));
    }

    #[test]
    fn unknown_word_returns_itself() {
        assert_eq!(get_synonyms("rustfmt"), vec!["rustfmt"]);
    }
}
