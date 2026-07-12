//! 统一上下文转换与缓存策略层（A2）。
//!
//! 抽出各 adapter 内联的格式处理，供 OpenAI / Anthropic 适配器复用：
//! - [`normalize_tool_schema`]：剥离 provider 冗余字段（`$schema`/`title`），输出干净 JSON Schema。
//! - [`CacheStrategy`] + [`inject_ephemeral_cache`] / [`anthropic_apply_cache`]：
//!   Anthropic 多点 `cache_control` breakpoint 注入（system + tools 末尾 + 倒数第二条消息），
//!   最大化 provider 端前缀缓存命中（Anthropic 允许最多 4 个 breakpoint）。
//!
//! 适配器只需构造「裸」请求体，再调用本模块注入缓存标记，逻辑集中、可测、可替换。
//! 新增 Provider 的格式处理（gemini-format / 自定义过滤）可继续在此扩展，避免散落到各 adapter。

use serde_json::{json, Value};

/// 前缀缓存注入策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheStrategy {
    /// 不注入 cache_control。
    None,
    /// 单点：仅 system 段（保持早期行为）。
    #[default]
    Ephemeral,
    /// 多点（Anthropic 推荐）：system + 最后一个 tool + 倒数第二条消息。
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
/// 3. `messages` 数组倒数第二条消息的最后 content 块（仅 MultiPoint，跳过最后一条通常是本轮新输入）
///
/// Anthropic 允许最多 4 个 breakpoint；本实现最多注入 3 个，留 1 个余量。
pub fn anthropic_apply_cache(body: &mut Value, strategy: CacheStrategy) {
    if !strategy.active() {
        return;
    }
    // 1. system 首块
    if let Some(first) = body.get_mut("system").and_then(Value::as_array_mut).and_then(|a| a.first_mut()) {
        inject_ephemeral_cache(first);
    }
    if strategy == CacheStrategy::MultiPoint {
        // 2. tools 末尾
        if let Some(last) = body.get_mut("tools").and_then(Value::as_array_mut).and_then(|a| a.last_mut()) {
            inject_ephemeral_cache(last);
        }
        // 3. messages 倒数第二条消息的末个 content 块
        inject_into_penultimate_message(body);
    }
}

/// 给 messages 倒数第二条消息的最后一个 content 块注入缓存标记。
fn inject_into_penultimate_message(body: &mut Value) {
    let Some(arr) = body.get_mut("messages").and_then(Value::as_array_mut) else { return };
    if arr.len() < 2 {
        return;
    }
    let idx = arr.len() - 2;
    let Some(msg) = arr.get_mut(idx) else { return };
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
        anthropic_apply_cache(&mut body, CacheStrategy::MultiPoint);
        assert_eq!(count_cache_breakpoints(&body), 3);
        // system 首块、tools 末个、messages 倒数第二条末块各一个
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][1]["content"][0]["cache_control"]["type"], "ephemeral");
        // 最后一条（本轮新输入）不应被标记
        assert!(body["messages"][2]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn ephemeral_only_marks_system() {
        let mut body = json!({"system":[{"type":"text","text":"s"}],"tools":[{"name":"t"}]});
        anthropic_apply_cache(&mut body, CacheStrategy::Ephemeral);
        assert_eq!(count_cache_breakpoints(&body), 1);
    }

    #[test]
    fn none_injects_nothing() {
        let mut body = json!({"system":[{"type":"text","text":"s"}]});
        anthropic_apply_cache(&mut body, CacheStrategy::None);
        assert_eq!(count_cache_breakpoints(&body), 0);
    }
}
