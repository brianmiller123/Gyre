//! In-band 工具调用方言：把工具规格渲染进 system prompt（**不**发原生 `tools`），
//! 并从模型的文本输出解析工具调用——供 function-calling 不稳的模型（GLM / DeepSeek 等
//! 兼容网关、本地 vLLM）用「提示词 + 文本协议」方式稳定调用工具。
//!
//! 移植 oh-my-pi owned-dialect 思路（renderInbandToolPrompt / 解析 in-band tool call），
//! 但用模型无关的 XML+JSON 协议，避免绑定特定 backend 的私有标记。

use agent_core::{ContentBlock, ToolSpec};

/// In-band 工具调用方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// XML 包裹的 JSON 工具调用（模型无关、可移植）。模型以
    /// `<tool_call>{"name":"<工具名>","arguments":{<参数JSON>}}</tool_call>` 发起调用。
    Xml,
}

impl Dialect {
    /// 渲染「工具规格 + 调用格式说明」为追加到 system prompt 的文本段。
    #[must_use]
    pub fn render_tools(self, tools: &[ToolSpec]) -> String {
        let mut s = String::new();
        s.push_str("\n\n## 工具调用（in-band 协议）\n");
        s.push_str(
            "你可以通过输出**工具调用块**来调用工具。每个调用块必须严格形如：\n\
             <tool_call>{\"name\":\"<工具名>\",\"arguments\":{<参数 JSON>}}</tool_call>\n\
             规则：\n\
             - 一次可输出多个调用块，每个独占一对 `<tool_call>`/`</tool_call>` 标签。\n\
             - `arguments` 必须是该工具参数 schema 的合法 JSON 对象。\n\
             - 仅在确实需要调用工具时输出该块；普通文本回复**不要**包含该标签。\n\
             可用工具：\n",
        );
        for t in tools {
            s.push_str(&format!(
                "\n<tool name=\"{}\">\n{}\n参数 schema: {}\n</tool>",
                t.name, t.description, t.schema
            ));
        }
        s
    }

    /// 从模型文本中解析工具调用，返回 `(工具调用内容块, 去除调用块后的纯文本)`。
    ///
    /// - 解析成功的块转为 [`ContentBlock::ToolCall`]，并从纯文本中移除其标签。
    /// - 解析失败（JSON 非法 / 缺字段）的块**保留为纯文本**（不丢失内容，便于模型自我纠正）。
    /// - 闭合标签缺失的尾段按纯文本保留（不解析半截调用）。
    #[must_use]
    pub fn parse_tool_calls(self, text: &str) -> (Vec<ContentBlock>, String) {
        match self {
            Self::Xml => parse_xml_tool_calls(text),
        }
    }
}

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

fn parse_xml_tool_calls(text: &str) -> (Vec<ContentBlock>, String) {
    // 单一真相源：批量解析 = 用流式解析器喂入整段 + 收尾。
    let mut p = XmlToolStreamParser::new();
    let (mut text_out, completed) = p.feed(text);
    let (tail, _) = p.finish();
    text_out.push_str(&tail);
    finalize_tool_calls(text_out, completed)
}

/// 把流式解析器收集到的「工具调用内联 JSON 列表」转为内容块；解析失败的按原文
/// 保留进 `cleaned`（可见、可纠正）。
pub(crate) fn finalize_tool_calls(
    mut cleaned: String,
    completed: Vec<String>,
) -> (Vec<ContentBlock>, String) {
    let mut calls = Vec::new();
    for (i, inner) in completed.iter().enumerate() {
        match parse_tool_call_json(inner, i) {
            Some(c) => calls.push(c),
            None => {
                cleaned.push_str(OPEN);
                cleaned.push_str(inner);
                cleaned.push_str(CLOSE);
            }
        }
    }
    (calls, cleaned)
}

/// 解析单个调用块内联 JSON：`{"name":"...","arguments":{...}}`。
pub(crate) fn parse_tool_call_json(inner: &str, idx: usize) -> Option<ContentBlock> {
    let v: serde_json::Value = serde_json::from_str(inner.trim()).ok()?;
    let name = v.get("name").and_then(serde_json::Value::as_str)?.to_string();
    let arguments = v
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    Some(ContentBlock::ToolCall {
        id: format!("inband_{idx}"),
        name,
        arguments,
    })
}

/// 跨 chunk 安全的 XML 工具调用流式解析器。
///
/// 增量喂入文本增量，**抑制** `<tool_call>...</tool_call>` 标记本身（不作为文本下发），
/// 仅下发标记外的普通文本；标记内的 JSON 在闭合后作为「完成的工具调用」交出，供
/// [`finalize_tool_calls`] 转为 `ContentBlock::ToolCall`。
///
/// 跨 chunk 安全：OPEN 未确认前，保留最长 `OPEN.len()-1` 字节的「可能是 OPEN 前缀」
/// 尾巴不冲刷，待后续增量确认（避免把被 chunk 切断的 `<tool_call` 当普通文本泄露）。
pub(crate) struct XmlToolStreamParser {
    in_tool: bool,
    buf: String,
}

impl XmlToolStreamParser {
    /// 构造。
    pub(crate) fn new() -> Self {
        Self {
            in_tool: false,
            buf: String::new(),
        }
    }

    /// 喂入一段文本增量，返回 `(可下发的普通文本, 本次闭合的工具调用内联 JSON 列表)`。
    pub(crate) fn feed(&mut self, delta: &str) -> (String, Vec<String>) {
        self.buf.push_str(delta);
        let mut text_out = String::new();
        let mut completed: Vec<String> = Vec::new();
        loop {
            if !self.in_tool {
                match self.buf.find(OPEN) {
                    Some(p) => {
                        text_out.push_str(&self.buf[..p]);
                        self.buf = self.buf[p + OPEN.len()..].to_string();
                        self.in_tool = true;
                        continue;
                    }
                    None => {
                        // 保留可能是 OPEN 前缀的尾巴；冲刷其余（在字符边界切，避免拆多字节）。
                        let mut cut = self.buf.len().saturating_sub(OPEN.len() - 1);
                        while cut > 0 && !self.buf.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        text_out.push_str(&self.buf[..cut]);
                        self.buf = self.buf[cut..].to_string();
                        break;
                    }
                }
            } else {
                match self.buf.find(CLOSE) {
                    Some(p) => {
                        completed.push(self.buf[..p].to_string());
                        self.buf = self.buf[p + CLOSE.len()..].to_string();
                        self.in_tool = false;
                        continue;
                    }
                    None => break, // 工具内部未闭合：全部保留，不下发。
                }
            }
        }
        (text_out, completed)
    }

    /// 收尾：返回剩余可下发文本（未闭合的工具调用按原文还原为可见文本，不产出 completed）。
    pub(crate) fn finish(&mut self) -> (String, Vec<String>) {
        if self.in_tool {
            let mut text = String::from(OPEN);
            text.push_str(&self.buf);
            self.buf.clear();
            self.in_tool = false;
            (text, Vec::new())
        } else {
            (std::mem::take(&mut self.buf), Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(name, format!("desc of {name}"), json!({ "type": "object" }))
    }

    #[test]
    fn render_lists_tools_and_format() {
        let s = Dialect::Xml.render_tools(&[spec("read_file"), spec("grep")]);
        assert!(s.contains("<tool_call>"), "应包含调用格式说明");
        assert!(s.contains("read_file") && s.contains("grep"), "应列出工具名");
        assert!(s.contains("参数 schema"), "应含 schema 标注");
    }

    #[test]
    fn parse_single_call() {
        let text = "let me read it\n<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}</tool_call>\n";
        let (calls, cleaned) = Dialect::Xml.parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            ContentBlock::ToolCall { id, name, arguments } => {
                assert!(id.starts_with("inband_"), "id 应为 inband 前缀: {id}");
                assert_eq!(name, "read_file");
                assert_eq!(arguments, &json!({"path":"a.txt"}));
            }
            other => panic!("应为 ToolCall: {other:?}"),
        }
        assert!(cleaned.contains("let me read it"), "周边文本应保留: {cleaned}");
        assert!(!cleaned.contains("<tool_call>"), "标签应被移除: {cleaned}");
    }

    #[test]
    fn parse_multiple_calls_preserve_order() {
        let text = "<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>\nmid\n<tool_call>{\"name\":\"b\",\"arguments\":{}}</tool_call>";
        let (calls, cleaned) = Dialect::Xml.parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(match &calls[0] { ContentBlock::ToolCall { name, .. } => name.clone(), _ => String::new() }, "a");
        assert_eq!(match &calls[1] { ContentBlock::ToolCall { name, .. } => name.clone(), _ => String::new() }, "b");
        assert_eq!(cleaned.trim(), "mid", "中间文本保留、标签移除: {cleaned}");
    }

    #[test]
    fn parse_no_call_returns_text_unchanged() {
        let text = "just a normal reply, no tools";
        let (calls, cleaned) = Dialect::Xml.parse_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn parse_invalid_json_kept_as_text() {
        // name 缺失 → 解析失败，块原样保留为文本。
        let text = "<tool_call>{\"arguments\":{}}</tool_call>";
        let (calls, cleaned) = Dialect::Xml.parse_tool_calls(text);
        assert!(calls.is_empty(), "非法调用不应产出 ToolCall");
        assert!(cleaned.contains("<tool_call>"), "非法块应保留可见: {cleaned}");
    }

    #[test]
    fn parse_unclosed_tag_kept_as_text() {
        let text = "partial <tool_call>{\"name\":\"a\"}";
        let (calls, cleaned) = Dialect::Xml.parse_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(cleaned, text, "未闭合标签按文本保留");
    }

    #[test]
    fn parse_missing_arguments_defaults_to_empty_object() {
        let text = "<tool_call>{\"name\":\"a\"}</tool_call>";
        let (calls, _) = Dialect::Xml.parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            ContentBlock::ToolCall { arguments, .. } => {
                assert_eq!(arguments, &serde_json::Value::Object(serde_json::Map::new()));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn stream_parser_split_marker_no_leak() {
        // <tool_call> 与 </tool_call> 均被切到多个 feed；解析器应跨 chunk 重组、
        // 不泄露任何标记，并在闭合后交出工具调用 JSON。
        let mut completed: Vec<String> = Vec::new();
        let mut p = XmlToolStreamParser::new();
        let (t, mut c) = p.feed("hi <tool");
        completed.append(&mut c);
        assert!(t.is_empty(), "未确认 OPEN 前应保留（可能是前缀）: {t:?}");
        let (t, mut c) = p.feed("_call>{\"name\":\"a\",\"arguments\":{}}</tool");
        completed.append(&mut c);
        assert_eq!(t, "hi ", "OPEN 确认后冲刷前置文本");
        let (t, mut c) = p.feed("_call> bye");
        completed.append(&mut c);
        assert!(t.is_empty(), "CLOSE 后的尾巴按前缀保留，待 finish");
        let (tail, _) = p.finish();
        assert_eq!(tail, " bye");
        let (calls, cleaned) = finalize_tool_calls(String::new(), completed);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            match &calls[0] {
                ContentBlock::ToolCall { name, .. } => name.clone(),
                _ => String::new(),
            },
            "a"
        );
        assert!(cleaned.is_empty());
    }
}
