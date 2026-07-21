//! 统一上下文转换与缓存策略层（A2）。
//!
//! 抽出各 adapter 内联的格式处理，供 OpenAI / Anthropic 适配器复用：
//! - [`normalize_tool_schema`]：剥离 provider 冗余字段（`$schema`/`title`），输出干净 JSON Schema。
//! - [`CacheStrategy`] + [`inject_ephemeral_cache`] / [`anthropic_apply_cache`]：
//!   Anthropic 多点 `cache_control` breakpoint 注入（system + tools 末尾 + 稳定前缀边界消息），
//!   按 `stable_prefix_len` 精确放置消息级 breakpoint（而非固定「倒数第二条」启发式），
//!   最大化 provider 端前缀缓存命中（Anthropic 允许最多 4 个 breakpoint）。
//!
//! 适配器只需构造「裸」请求体，再调用本模块注入缓存标记，逻辑集中、可测、可替换。
//! 新增 Provider 的格式处理（gemini-format / 自定义过滤）可继续在此扩展，避免散落到各 adapter。

use serde_json::{Value, json};

/// 前缀缓存注入策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheStrategy {
    /// 不注入 cache_control。
    None,
    /// 单点：仅 system 段（保持早期行为）。
    #[default]
    Ephemeral,
    /// 多点（Anthropic 推荐）：system + 最后一个 tool + 稳定前缀边界消息。
    MultiPoint,
}

impl CacheStrategy {
    /// 是否注入任何缓存标记。
    #[must_use]
    pub const fn active(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// 给单个 JSON 块注入 `cache_control: { type: "ephemeral" }`（原地修改，幂等）。
pub fn inject_ephemeral_cache(block: &mut Value) {
    if let Value::Object(map) = block {
        map.insert("cache_control".to_string(), json!({"type":"ephemeral"}));
    }
}

/// 规范化工具 schema：剥离部分 provider 不支持/冗余字段（`$schema`、`title`）。
/// 返回新值，不改原值。
#[must_use]
pub fn normalize_tool_schema(schema: &Value) -> Value {
    let mut v = schema.clone();
    if let Value::Object(map) = &mut v {
        map.remove("$schema");
        map.remove("title");
    }
    v
}

/// 构造 Anthropic system 块数组（按策略注入 cache_control）。
#[must_use]
pub fn anthropic_system_blocks(system: &[String], strategy: CacheStrategy) -> Value {
    let text = system.join("\n\n");
    let mut block = json!({"type":"text","text":text});
    if strategy.active() {
        inject_ephemeral_cache(&mut block);
    }
    Value::Array(vec![block])
}

/// 对已构造好的 Anthropic 请求体应用多点缓存策略（原地修改）。
///
/// 注入点（受 [`CacheStrategy`] 控制）：
/// 1. `system` 数组首块（Ephemeral / MultiPoint）
/// 2. `tools` 数组末个工具（仅 MultiPoint）
/// 3. `messages` 中**稳定前缀边界**消息的最后 content 块（仅 MultiPoint）：
///    目标索引 = `stable_prefix_len - 1`（稳定前缀最后一条），但 clamp 到不超过倒数第二条
///    （最后一条为本轮新输入，永不缓存）。`stable_prefix_len == 0`（首次/压缩/分支切换后）
///    时**跳过消息注入**——全量重发，放 breakpoint 也命中不了，避免浪费配额。
///
/// Anthropic 允许最多 4 个 breakpoint；本实现最多注入 3 个，留 1 个余量。
pub fn anthropic_apply_cache(body: &mut Value, strategy: CacheStrategy, stable_prefix_len: usize) {
    if !strategy.active() {
        return;
    }
    // 1. system 首块
    if let Some(first) = body
        .get_mut("system")
        .and_then(Value::as_array_mut)
        .and_then(|a| a.first_mut())
    {
        inject_ephemeral_cache(first);
    }
    if strategy == CacheStrategy::MultiPoint {
        // 2. tools 末尾
        if let Some(last) = body
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .and_then(|a| a.last_mut())
        {
            inject_ephemeral_cache(last);
        }
        // 3. 稳定前缀边界消息的末个 content 块
        inject_into_stable_prefix_message(body, stable_prefix_len);
    }
}

/// 给稳定前缀边界消息的最后一个 content 块注入缓存标记。
///
/// 目标索引 = `stable_prefix_len - 1`（稳定前缀最后一条），但 clamp 到不超过倒数第二条
/// （`len - 2`；最后一条为本轮新输入）。`stable_prefix_len == 0` 或消息不足 2 条时跳过。
fn inject_into_stable_prefix_message(body: &mut Value, stable_prefix_len: usize) {
    let Some(arr) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    if arr.len() < 2 || stable_prefix_len == 0 {
        // 无稳定前缀（压缩/分支切换后）：全量重发，不在消息上放 breakpoint。
        return;
    }
    // 稳定前缀最后一条索引（0-based）；不超过倒数第二条（len-2）。
    let candidate = stable_prefix_len - 1;
    let target = candidate.min(arr.len() - 2);
    let Some(msg) = arr.get_mut(target) else {
        return;
    };
    if let Some(blocks) = msg.get_mut("content").and_then(Value::as_array_mut) {
        if let Some(last_block) = blocks.last_mut() {
            inject_ephemeral_cache(last_block);
        }
    }
}

/// 统计 body 内 cache_control 标记数量（可观测/测试用）。
#[must_use]
pub fn count_cache_breakpoints(body: &Value) -> usize {
    let ser = serde_json::to_string(body).unwrap_or_default();
    ser.matches("\"cache_control\"").count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_schema_and_title() {
        let schema = json!({"$schema":"http://...","title":"X","type":"object","properties":{}});
        let out = normalize_tool_schema(&schema);
        assert!(out.get("$schema").is_none());
        assert!(out.get("title").is_none());
        assert_eq!(out["type"], "object");
    }

    #[test]
    fn system_block_carries_cache_when_active() {
        let blocks = anthropic_system_blocks(&["hi".into()], CacheStrategy::Ephemeral);
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");

        let none = anthropic_system_blocks(&["hi".into()], CacheStrategy::None);
        assert!(none[0].get("cache_control").is_none());
    }

    #[test]
    fn multipoint_injects_three_breakpoints() {
        let mut body = json!({
            "system": [{"type":"text","text":"s"}],
            "tools": [{"name":"t1"},{"name":"t2"}],
            "messages": [
                {"role":"user","content":[{"type":"text","text":"a"}]},
                {"role":"assistant","content":[{"type":"text","text":"b"}]},
                {"role":"user","content":[{"type":"text","text":"c"}]}
            ]
        });
        // 稳定前缀覆盖前 2 条 → breakpoint 落在 messages[1]（倒数第二条），与旧行为一致。
        anthropic_apply_cache(&mut body, CacheStrategy::MultiPoint, 2);
        assert_eq!(count_cache_breakpoints(&body), 3);
        // system 首块、tools 末个、稳定前缀边界消息末块各一个
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        // 最后一条（本轮新输入）不应被标记
        assert!(
            body["messages"][2]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn multipoint_skips_message_breakpoint_when_no_stable_prefix() {
        // 压缩 / 分支切换后 stable_prefix_len == 0：全量重发，不在消息上放 breakpoint，
        // 只保留 system + tools 两个（放也命中不了，避免浪费配额）。
        let mut body = json!({
            "system": [{"type":"text","text":"s"}],
            "tools": [{"name":"t1"},{"name":"t2"}],
            "messages": [
                {"role":"user","content":[{"type":"text","text":"a"}]},
                {"role":"assistant","content":[{"type":"text","text":"b"}]},
                {"role":"user","content":[{"type":"text","text":"c"}]}
            ]
        });
        anthropic_apply_cache(&mut body, CacheStrategy::MultiPoint, 0);
        assert_eq!(count_cache_breakpoints(&body), 2);
        assert!(
            body["messages"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn multipoint_message_breakpoint_tracks_stable_prefix() {
        // 5 条消息但只有前 2 条稳定（steering 注入后）：breakpoint 应前移到 messages[1]，
        // 而非固定落在倒数第二条 messages[3]。
        let mut body = json!({
            "system": [{"type":"text","text":"s"}],
            "tools": [{"name":"t1"}],
            "messages": [
                {"role":"user","content":[{"type":"text","text":"m0"}]},
                {"role":"assistant","content":[{"type":"text","text":"m1"}]},
                {"role":"user","content":[{"type":"text","text":"m2"}]},
                {"role":"assistant","content":[{"type":"text","text":"m3"}]},
                {"role":"user","content":[{"type":"text","text":"m4_new"}]}
            ]
        });
        anthropic_apply_cache(&mut body, CacheStrategy::MultiPoint, 2);
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        // 不稳定区（m2/m3）与本轮新输入（m4）都不应被标记。
        assert!(
            body["messages"][2]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert!(
            body["messages"][3]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn ephemeral_only_marks_system() {
        let mut body = json!({"system":[{"type":"text","text":"s"}],"tools":[{"name":"t"}]});
        anthropic_apply_cache(&mut body, CacheStrategy::Ephemeral, 0);
        assert_eq!(count_cache_breakpoints(&body), 1);
    }

    #[test]
    fn none_injects_nothing() {
        let mut body = json!({"system":[{"type":"text","text":"s"}]});
        anthropic_apply_cache(&mut body, CacheStrategy::None, 0);
        assert_eq!(count_cache_breakpoints(&body), 0);
    }
}
