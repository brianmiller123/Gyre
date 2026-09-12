//! # `agent --rpc`：NDJSON 行协议模式
//!
//! 供外部语言 / 机器人集成的 stdio 服务面（对标 oh-my-pi `--mode rpc` 的最小集）：
//! stdin 逐行读 JSON 请求，stdout 逐行写 JSON 事件 / 响应。进程内一个 Agent 实例与同一份
//! Context 跨多个 prompt 请求复用（同一会话累积上下文）。协议契约见 `docs/rpc.md`。
//!
//! 实现要点：
//! - 装配镜像 `main()` 的单次任务路径（Provider / Tools / Context / Approval / TTSR /
//!   advisor / 长期记忆），复用 crate 根共享的 `assemble_builtin_tools`、
//!   `optional_context_files`、`load_skill_catalog`、`apply_model_switch` 等辅助。
//! - 取消：每轮经 [`agent::Agent::run_with_cancel`] 派生独立取消令牌，`cancel` 请求 /
//!   SIGINT 命中即中断流式与在途工具（Agent 与 Context 均可继续下一轮）。
//! - 审批：stdin 是协议通道，无法交互审批——一律拒绝（写工具会失败并反映在 `tool_result`）；
//!   需要全自动写权限时以 `--approval-mode yolo` 启动。
//! - 会话控制：`get_state` / `set_model` / `set_thinking` / `get_messages` / `compact` /
//!   `get_usage`（对标 oh-my-pi `RpcCommand` 的最小会话面）。查询类空闲与运行中均可受理；
//!   变更型命令（set_model / compact）仅空闲受理；`set_thinking` 经 RuntimeOverrides
//!   每轮解析，运行期调整即时生效、无需重建 Agent。
//! - 握手与分帧（对齐 oh-my-pi `RpcReadyFrame` / `RpcChunkFrame`）：连接建立后、处理
//!   任何请求前先发 `ready` 帧；超 `MAX_RPC_FRAME_BYTES` 的出站逻辑帧自动按 `rpc_chunk`
//!   分片发送，入站 `rpc_chunk` 序列经 [`RpcFrameDecoder`] 重组（普通帧直通，向后兼容）。
//! - 所有日志走 stderr（telemetry 已确保写 stderr），stdout 仅协议行，单行 JSON（无内嵌换行）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agent_core::{
    AgentEvent, AgentMessage, AskKind, AskMessage, AskResponse, CompactionStrategy, Usage,
};
use agent_i18n::t;
use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures::StreamExt;
use secrecy::ExposeSecret;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::compiled_minimizer;
use crate::{
    AutoRetainHook, ConsolidateHook, StructuredSleepHook, apply_model_switch,
    assemble_builtin_tools, load_skill_catalog, optional_context_files,
};

// ──────────────────────────────────────────────────────────────────────────────
// 协议层（纯数据 / 纯函数，可单测）
// ──────────────────────────────────────────────────────────────────────────────

/// 单个 stdin 请求行。
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct RpcRequest {
    /// 请求类型：prompt / cancel / ping + 会话控制命令（get_state / set_model /
    /// set_thinking / get_messages / compact / get_usage；未知类型由调用方按 id 回错误）。
    #[serde(rename = "type")]
    ty: String,
    /// 请求 id（响应 / 事件**原样回带**的任意 JSON 值）。
    ///
    /// H9：此前强制 `u64`，omp 官方客户端发字符串 id（`{id:"…"}`）会因反序列化失败被
    /// 整帧丢弃 → 无法握手。现接受字符串 / 数字 / 缺失，缺失时按 `null` 回带。
    #[serde(default)]
    id: Option<serde_json::Value>,
    /// prompt 的文本（prompt 请求必填，非空）。
    ///
    /// `message` 为 omp 兼容别名（omp `RpcCommand.prompt.message`）。
    #[serde(default, alias = "message")]
    text: Option<String>,
    /// 模型别名（可选；非空时 prompt 前尝试切换 profile）。
    #[serde(default)]
    model: Option<String>,
    /// set_model 的目标别名（缺失 / 空串 → 按 id 回错误帧）。
    #[serde(default)]
    alias: Option<String>,
    /// set_thinking 的思考预算：数字 = 预算（token），`null` = 关闭思考；
    /// 缺失 / 类型非法 → 按 id 回错误帧。宽松 Value 承载，校验在命令层。
    /// `deserialize_some`：serde 对 `Option<T>` 会把 JSON `null` 归约成 `None`，
    /// 与字段缺失不可区分——这里显式包装，`null` 保留为 `Some(Value::Null)`
    /// （set_thinking 的「关闭思考」语义依赖该区分）。
    #[serde(default, deserialize_with = "deserialize_some")]
    budget: Option<serde_json::Value>,
    /// get_messages 的条数上限（缺失 → 默认 [`DEFAULT_MESSAGES_LIMIT`]；非法 → 错误帧）。
    #[serde(default)]
    limit: Option<serde_json::Value>,
    /// switch_branch 的目标节点（完整 id 或 ≥4 字符前缀；缺失/空 → 错误帧）。
    #[serde(default)]
    node: Option<String>,
    /// switch_branch 是否走摘要交接（true → `switch_branch_with_handoff`；缺省 false）。
    #[serde(default)]
    handoff: Option<bool>,
    /// 宿主对反向请求的回答负载（`type:"response"` 时有效；缺失按拒绝处理）。
    #[serde(default)]
    result: Option<serde_json::Value>,
    /// `set_todos` 的清单负载（omp `phases` 或 Gyre `items`；H7）。宽松承载，
    /// 形状校验在命令层（非法 → error 帧）。
    #[serde(default)]
    pub(crate) todos: Option<serde_json::Value>,
    /// `negotiate_protocol` 的目标协议版本（H6；omp `{type:"negotiate_protocol",
    /// protocolVersion:2}`）。宽松承载，合法性校验在命令层（非法 → success:false）。
    #[serde(
        default,
        rename = "protocolVersion",
        alias = "protocol_version",
        alias = "protocol_version_id"
    )]
    protocol_version: Option<serde_json::Value>,
}

/// serde 辅助：`Option<T>` 字段把 JSON `null` 反序列化为 `Some(Value::Null)`
/// 而非 `None`（配合 `#[serde(default)]`，缺失仍为 `None`），使「显式 null」
/// 与「字段缺失」可区分（set_thinking / get_messages 的参数语义依赖）。
fn deserialize_some<'de, D>(de: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

/// 单个 stdout 响应行（所有行均为单行 JSON，字段按类型取舍）。
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
struct RpcEnvelope {
    #[serde(rename = "type")]
    ty: &'static str,
    /// 请求 id 原样回带（H9：字符串或数字；缺省 `null`）。
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// `get_state` 响应负载（camelCase 对齐 omp `RpcSessionState` 的最小子集）。
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<serde_json::Value>,
    /// `get_messages` 响应负载（`{role,text}` 预览数组）。
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<serde_json::Value>,
    /// `set_model` 成功后的模型 id。
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// set_thinking 成功后的思考预算（`null` = 已关闭思考；`None` 不序列化）。
    /// 三态：`None`=不涉及 / `Some(None)`=显式关闭 / `Some(Some(n))`=预算 n，定向豁免。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[allow(clippy::option_option)]
    budget: Option<Option<usize>>,
    /// `get_tree` 响应负载（节点森林数组）。
    #[serde(skip_serializing_if = "Option::is_none")]
    tree: Option<serde_json::Value>,
    /// `list_models` 响应负载（`{id,ownedBy}` 数组）。
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<serde_json::Value>,
}

impl RpcEnvelope {
    /// 取请求 id 用于回带（缺失 → `null`，H9）。
    fn req_id(req: &RpcRequest) -> serde_json::Value {
        req.id.clone().unwrap_or(serde_json::Value::Null)
    }

    /// 事件行：`{"type":"event","id":N,"event":{…}}`。
    fn event(id: impl Into<serde_json::Value>, event: serde_json::Value) -> Self {
        Self {
            ty: "event",
            id: id.into(),
            event: Some(event),
            ..Self::default()
        }
    }

    /// 成功收尾：`{"type":"done","id":N,"ok":true,"usage":{…},"turns":M}`。
    fn done_ok(id: impl Into<serde_json::Value>, usage: &Usage, turns: u64) -> Self {
        Self {
            ty: "done",
            id: id.into(),
            ok: Some(true),
            usage: Some(usage_json(usage)),
            turns: Some(turns),
            ..Self::default()
        }
    }

    /// 失败收尾：`{"type":"done","id":N,"ok":false,"error":"…"}`。
    fn done_err(id: impl Into<serde_json::Value>, error: impl Into<String>) -> Self {
        Self {
            ty: "done",
            id: id.into(),
            ok: Some(false),
            error: Some(error.into()),
            ..Self::default()
        }
    }

    /// 错误响应（未知类型 / 缺参 / 模型切换失败 / 无效 JSON 等）：
    /// `{"type":"error","id":N,"message":"…"}`。
    fn error(id: impl Into<serde_json::Value>, message: impl Into<String>) -> Self {
        Self {
            ty: "error",
            id: id.into(),
            message: Some(message.into()),
            ..Self::default()
        }
    }

    /// ping 应答：`{"type":"pong","id":N}`。
    fn pong(id: impl Into<serde_json::Value>) -> Self {
        Self {
            ty: "pong",
            id: id.into(),
            ..Self::default()
        }
    }

    /// 通用成功应答：`{"type":"ok","id":N,"ok":true}`（compact 受理等）。
    fn ok(id: impl Into<serde_json::Value>) -> Self {
        Self {
            ty: "ok",
            id: id.into(),
            ok: Some(true),
            ..Self::default()
        }
    }

    /// set_model 成功（重建 Agent 后由调用方回）：
    /// `{"type":"ok","id":N,"ok":true,"model":"<id>"}`。
    fn ok_model(id: impl Into<serde_json::Value>, model_id: &str) -> Self {
        Self {
            ty: "ok",
            id: id.into(),
            ok: Some(true),
            model: Some(model_id.to_string()),
            ..Self::default()
        }
    }

    /// set_thinking 成功：`{"type":"ok","id":N,"ok":true,"budget":N}`；
    /// `budget: null` 表示已关闭思考。
    fn ok_budget(id: impl Into<serde_json::Value>, budget: Option<usize>) -> Self {
        Self {
            ty: "ok",
            id: id.into(),
            ok: Some(true),
            budget: Some(budget),
            ..Self::default()
        }
    }

    /// get_state 成功：`{"type":"state","id":N,"state":{…}}`。
    fn state(id: impl Into<serde_json::Value>, state: serde_json::Value) -> Self {
        Self {
            ty: "state",
            id: id.into(),
            state: Some(state),
            ..Self::default()
        }
    }

    /// get_messages 成功：`{"type":"messages","id":N,"messages":[…]}`。
    fn messages(id: impl Into<serde_json::Value>, messages: serde_json::Value) -> Self {
        Self {
            ty: "messages",
            id: id.into(),
            messages: Some(messages),
            ..Self::default()
        }
    }

    /// get_usage 成功：`{"type":"usage","id":N,"usage":{…}}`（累计用量，同 `done.usage` 形状）。
    fn usage(id: impl Into<serde_json::Value>, usage: serde_json::Value) -> Self {
        Self {
            ty: "usage",
            id: id.into(),
            usage: Some(usage),
            ..Self::default()
        }
    }

    /// get_tree 成功：`{"type":"tree","id":N,"tree":[…]}`（节点森林数组）。
    fn tree(id: impl Into<serde_json::Value>, nodes: serde_json::Value) -> Self {
        Self {
            ty: "tree",
            id: id.into(),
            tree: Some(nodes),
            ..Self::default()
        }
    }

    /// list_models 成功：`{"type":"models","id":N,"models":[…]}`。
    fn models_list(id: impl Into<serde_json::Value>, models: serde_json::Value) -> Self {
        Self {
            ty: "models",
            id: id.into(),
            models: Some(models),
            ..Self::default()
        }
    }
}

/// 用量对象：`{"input":N,"output":N,"cache_read":N,"cache_write":N,"cost":f}`。
/// cost 非有限值（NaN/Inf）时归零，保证单行 JSON 可序列化。
fn usage_json(u: &Usage) -> serde_json::Value {
    serde_json::json!({
        "input": u.input_tokens,
        "output": u.output_tokens,
        "cache_read": u.cache_read_tokens,
        "cache_write": u.cache_write_tokens,
        "cost": if u.cost_usd.is_finite() { u.cost_usd } else { 0.0 },
    })
}

/// 把 agent 事件映射为 RPC 事件对象；`None` 表示该事件不对外暴露。
///
/// **旧式（v1）形状**：`{"type":"event","event":{"kind":…}}`——保留 `kind` 判别字段，
/// 但 H8 起补齐此前丢失的 `id` / `result`（工具调用 id 与结构化结果）。
///
/// omp 兼容形状见 [`map_event_omp`]（协商 v2 的对端使用）。
fn map_event(ev: &AgentEvent) -> Option<serde_json::Value> {
    match ev {
        AgentEvent::TextDelta(t) => Some(serde_json::json!({"kind": "text_delta", "text": t})),
        AgentEvent::ThinkingDelta(t) => {
            Some(serde_json::json!({"kind": "thinking_delta", "text": t}))
        }
        AgentEvent::Say(s) => Some(serde_json::json!({"kind": "status", "text": s.text})),
        // 会话级结构化事件（压缩 / 重试 / 模型回退）：与 status 双通道，`event.type`
        // 为判别字段（omp `AgentSessionEvent` 同形）。展示文本仍在 status 事件里。
        AgentEvent::Session(ev) => Some(serde_json::json!({"kind": "session", "event": ev})),
        // H8：补 `id`（tool_call_id）——此前只发 name，消费者无法把结果配回调用。
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            name,
            args,
        } => Some(serde_json::json!({
            "kind": "tool_call",
            "id": tool_call_id,
            "name": name,
            "args": args,
        })),
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            name,
            partial,
        } => Some(serde_json::json!({
            "kind": "tool_update",
            "id": tool_call_id,
            "name": name,
            "partial": partial,
        })),
        // H8：补 `id` 与结构化 `result`（此前只有 name/ok，工具输出在 RPC 面完全丢失）。
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            name,
            result,
            is_error,
        } => Some(serde_json::json!({
            "kind": "tool_result",
            "id": tool_call_id,
            "name": name,
            "ok": !is_error,
            "result": result,
        })),
        AgentEvent::Usage(u) => {
            let mut v = usage_json(u);
            v["kind"] = serde_json::Value::String("usage".into());
            Some(v)
        }
        // H8：轮次 / 消息边界此前完全未暴露，消费者无法划分边界。
        AgentEvent::TurnStart => Some(serde_json::json!({"kind": "turn_start"})),
        AgentEvent::TurnEnd {
            message,
            tool_results,
            will_continue,
        } => Some(serde_json::json!({
            "kind": "turn_end",
            "message": message,
            "toolResults": tool_results,
            "willContinue": will_continue,
        })),
        AgentEvent::MessageStart => Some(serde_json::json!({"kind": "message_start"})),
        AgentEvent::MessageEnd(message) => {
            Some(serde_json::json!({"kind": "message_end", "message": message}))
        }
        _ => None,
    }
}

/// omp 兼容事件行（H8；仅对**已协商 v2** 的对端使用）。
///
/// 形状对齐 omp `agent/types.ts:868-888` 的 `AgentEvent`：顶层 `type` 即判别字段，
/// 工具生命周期用 `toolCallId`/`toolName`/`args`/`result`/`isError`（camelCase）。
///
/// 明确的两处偏差（Gyre 侧成本/能力约束，客户端可容忍）：
/// - `message_update` 不带 `message` partial 快照——Gyre 的流式路径刻意不做每 delta
///   整条消息克隆（见 `AgentEvent` 文档）；客户端按 `assistantMessageEvent.delta` 自行累积；
/// - 无 `agent_start`（Gyre 无显式起始事件）；`Done` 映射为 `agent_end` + `summary`。
fn map_event_omp(ev: &AgentEvent) -> Option<serde_json::Value> {
    Some(match ev {
        AgentEvent::TextDelta(t) => serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "text_delta", "delta": t},
        }),
        AgentEvent::ThinkingDelta(t) => serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "thinking_delta", "delta": t},
        }),
        AgentEvent::Say(s) => serde_json::json!({
            "type": "status",
            "message": s.text,
            "level": format!("{:?}", s.kind).to_lowercase(),
        }),
        // SessionEvent 自身即带 `type` 判别字段 → 直接扁平外发（与 omp `output(event)` 一致）。
        AgentEvent::Session(ev) => serde_json::to_value(ev).ok()?,
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            name,
            args,
        } => serde_json::json!({
            "type": "tool_execution_start",
            "toolCallId": tool_call_id,
            "toolName": name,
            "args": args,
        }),
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            name,
            partial,
        } => serde_json::json!({
            "type": "tool_execution_update",
            "toolCallId": tool_call_id,
            "toolName": name,
            "partialResult": partial,
        }),
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            name,
            result,
            is_error,
        } => serde_json::json!({
            "type": "tool_execution_end",
            "toolCallId": tool_call_id,
            "toolName": name,
            "result": result,
            "isError": is_error,
        }),
        AgentEvent::TurnStart => serde_json::json!({"type": "turn_start"}),
        AgentEvent::TurnEnd {
            message,
            tool_results,
            will_continue,
        } => serde_json::json!({
            "type": "turn_end",
            "message": message,
            "toolResults": tool_results,
            "willContinue": will_continue,
        }),
        AgentEvent::MessageStart => serde_json::json!({"type": "message_start"}),
        AgentEvent::MessageEnd(message) => {
            serde_json::json!({"type": "message_end", "message": message})
        }
        AgentEvent::Usage(u) => {
            let mut v = usage_json(u);
            v["type"] = serde_json::Value::String("usage".into());
            v
        }
        AgentEvent::Done(summary) => {
            serde_json::json!({"type": "agent_end", "summary": summary})
        }
        AgentEvent::Error(message) => {
            serde_json::json!({"type": "error", "message": message})
        }
        // 兼容别名（旧式 ToolExec 进度）：映射为 omp 的工具进度帧。
        AgentEvent::ToolExec { name, output } => serde_json::json!({
            "type": "tool_execution_update",
            "toolName": name,
            "partialResult": output,
        }),
        AgentEvent::StateChanged(state) => {
            serde_json::json!({"type": "state_changed", "state": format!("{state:?}").to_lowercase()})
        }
        _ => return None,
    })
}

/// 写出一个 agent 事件行（H8）：v2 对端 → omp 形状**裸帧**（顶层 `type`）；否则旧式
/// `{"type":"event","id":N,"event":{…}}` 包装。无映射的事件静默跳过。
async fn write_agent_event<W: tokio::io::AsyncWrite + Unpin>(
    out: &mut W,
    id: &serde_json::Value,
    ev: &AgentEvent,
) -> Result<()> {
    if RPC_V2_NEGOTIATED.load(Ordering::Relaxed) {
        if let Some(v) = map_event_omp(ev) {
            write_frame(out, &v).await?;
        }
    } else if let Some(v) = map_event(ev) {
        write_frame(out, &RpcEnvelope::event(id.clone(), v)).await?;
    }
    Ok(())
}

/// 序列化为单行 JSON 文本（不含行尾换行；文本内换行由 serde 自动转义，保证一行一条消息）。
fn encode_json<T: serde::Serialize>(env: &T) -> Result<String> {
    serde_json::to_string(env).context("序列化 RPC 响应失败")
}

// ── 握手与分帧（对齐 oh-my-pi `rpc-types.ts` / `rpc-frame.ts`）──────────────

/// 当前协议版本（`ready.protocolVersion`，对齐 omp `RpcReadyFrame`）。
pub(crate) const PROTOCOL_VERSION: u64 = 1;
/// 服务端支持的协议版本（`ready.supportedProtocolVersions`；v2 = `rpc_chunk` 分帧）。
pub(crate) const SUPPORTED_PROTOCOL_VERSIONS: [u64; 2] = [1, 2];
/// 单条 NDJSON 物理行（含换行）的最大 UTF-8 字节数（对齐 omp `MAX_RPC_FRAME_BYTES`）。
pub(crate) const MAX_RPC_FRAME_BYTES: usize = 1024 * 1024;
/// v2 分片重组后单个逻辑帧的最大 UTF-8 字节数（对齐 omp `MAX_RPC_REASSEMBLED_BYTES`）。
pub(crate) const MAX_RPC_REASSEMBLED_BYTES: usize = 64 * 1024 * 1024;
/// 单个 `rpc_chunk` 携带的最大原始字节数（base64 编码前的 UTF-8 字节，对齐 omp 同名常量）。
pub(crate) const RPC_CHUNK_PAYLOAD_BYTES: usize = 256 * 1024;
/// 并发在途分片序列上限：NDJSON 单管道内序列实际不会交错，仅防御异常对端堆积。
const MAX_PENDING_CHUNK_SEQUENCES: usize = 8;

/// 发送侧分片 id 计数器（对齐 omp `rpc-${++counter}` 命名）。
static RPC_CHUNK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// v2 分帧是否已由对端经 `negotiate_protocol` 协商启用（H6 门控）。
///
/// omp `rpc-frame.ts:293-296` 只在 `protocolVersion === 2` 时才发 `rpc_chunk`；
/// 此前 Gyre 无条件分片——v1 客户端会把 `rpc_chunk` 当未知类型丢弃，长帧直接丢失。
/// `agent --rpc` 每进程只服务一个对端（单 stdin/stdout 通道），故用进程级原子。
static RPC_V2_NEGOTIATED: AtomicBool = AtomicBool::new(false);

/// 测试串行锁：改动 [`RPC_V2_NEGOTIATED`] 的用例必须持锁，
/// 否则并行测试会互相把门控开关改回 false（进程级状态在测试中天然竞态）。
#[cfg(test)]
static CHUNK_GATE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// omp 兼容响应帧（H9）：`{id, type:"response", command, success, data?|error?, code?}`。
///
/// 仅用于 omp 风格命令（当前为 `negotiate_protocol`）；Gyre 既有命令继续用
/// [`RpcEnvelope`]（向后兼容 `docs/rpc.md` 的既有契约）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct OmpResponseFrame {
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    id: serde_json::Value,
    #[serde(rename = "type")]
    ty: &'static str,
    command: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

impl OmpResponseFrame {
    /// 成功响应（无 data 时不序列化 `data` 字段，对齐 omp `success()`）。
    fn success(
        id: serde_json::Value,
        command: impl Into<String>,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self {
            id,
            ty: "response",
            command: command.into(),
            success: true,
            data,
            error: None,
            code: None,
        }
    }

    /// 失败响应（对齐 omp `error()`）。
    fn failure(
        id: serde_json::Value,
        command: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id,
            ty: "response",
            command: command.into(),
            success: false,
            data: None,
            error: Some(message.into()),
            code: None,
        }
    }
}

/// 握手帧（对齐 omp `RpcReadyFrame`）：连接建立后、处理任何请求前的首行输出。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RpcReadyFrame {
    #[serde(rename = "type")]
    ty: &'static str,
    protocol_version: u64,
    supported_protocol_versions: &'static [u64],
    max_frame_bytes: usize,
    max_reassembled_frame_bytes: usize,
}

impl RpcReadyFrame {
    /// 以服务端协议常量构造 ready 帧。
    fn new() -> Self {
        Self {
            ty: "ready",
            protocol_version: PROTOCOL_VERSION,
            supported_protocol_versions: &SUPPORTED_PROTOCOL_VERSIONS,
            max_frame_bytes: MAX_RPC_FRAME_BYTES,
            max_reassembled_frame_bytes: MAX_RPC_REASSEMBLED_BYTES,
        }
    }
}

/// 出站分片帧（对齐 omp `RpcChunkFrame`）：`data` 为原始 UTF-8 字节的 base64，
/// `byte_length` 为整个逻辑帧（不含换行）的字节数。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RpcChunkFrame<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    chunk_id: &'a str,
    index: usize,
    count: usize,
    byte_length: usize,
    data: &'a str,
}

/// 把已序列化的逻辑帧（JSON 文本，不含换行）编码为一条或多条物理行（每行以 `\n` 结尾）：
/// 未超 [`MAX_RPC_FRAME_BYTES`] 时原样单行直通（向后兼容 v1 客户端）；超限时按
/// [`RPC_CHUNK_PAYLOAD_BYTES`] 切片为懒生成的 `rpc_chunk` 序列（对齐 omp v2；逐片生成，
/// 不在内存中持有整份 base64 传输体积）；超出重组上限时回退 `rpc_frame_error` 行
/// （对齐 omp `overflowFrame`）。
fn encode_frame_lines(json: &str) -> Box<dyn Iterator<Item = String> + '_> {
    let bytes = json.as_bytes();
    if bytes.len() < MAX_RPC_FRAME_BYTES {
        return Box::new(std::iter::once(format!("{json}\n")));
    }
    // H6：v1 客户端不认 `rpc_chunk`（会当未知类型丢弃）→ 未协商 v2 时绝不分片。
    if !RPC_V2_NEGOTIATED.load(Ordering::Relaxed) {
        return Box::new(std::iter::once(
            "{\"type\":\"rpc_frame_error\",\"error\":\"RPC frame exceeded the transport limit; renegotiate with negotiate_protocol v2\"}\n"
                .to_string(),
        ));
    }
    if bytes.len() > MAX_RPC_REASSEMBLED_BYTES {
        // 超出重组上限：放弃传输，回退错误帧。
        return Box::new(std::iter::once(
            "{\"type\":\"rpc_frame_error\",\"error\":\"RPC frame exceeded the transport limit\"}\n"
                .to_string(),
        ));
    }
    let count = bytes.len().div_ceil(RPC_CHUNK_PAYLOAD_BYTES);
    let chunk_id = format!("rpc-{}", RPC_CHUNK_COUNTER.fetch_add(1, Ordering::Relaxed));
    Box::new((0..count).map(move |index| {
        let start = index * RPC_CHUNK_PAYLOAD_BYTES;
        let end = std::cmp::min(start + RPC_CHUNK_PAYLOAD_BYTES, bytes.len());
        let chunk = RpcChunkFrame {
            ty: "rpc_chunk",
            chunk_id: &chunk_id,
            index,
            count,
            byte_length: bytes.len(),
            data: &BASE64_STANDARD.encode(&bytes[start..end]),
        };
        format!(
            "{}\n",
            serde_json::to_string(&chunk).expect("rpc_chunk 序列化不可失败")
        )
    }))
}

/// 单个 chunkId 的重组缓冲：按 index 槽位缓存，集齐 count 个唯一分片后交付。
struct PendingChunks {
    count: usize,
    byte_length: usize,
    slots: Vec<Option<Vec<u8>>>,
    received: usize,
    received_bytes: usize,
}

/// 入站 `rpc_chunk` 重组器（接收方向，对齐 omp `RpcFrameDecoder`，接收侧放宽为乱序
/// 容忍）：按 chunkId 聚合、按 index 排序拼接、count 到齐交付；同一 `(chunkId, index)`
/// 重复帧忽略；不带分片字段的普通帧直通（向后兼容旧客户端）。
#[derive(Default)]
struct RpcFrameDecoder {
    pending: HashMap<String, PendingChunks>,
}

impl RpcFrameDecoder {
    /// 推入一帧已解析的 JSON Value。`Ok(Some(逻辑帧))` 可继续处理；`Ok(None)` 表示
    /// 分片已缓存、序列未齐；`Err(协议错误文案)` 需回 error 行（残片就地丢弃以恢复）。
    fn push(
        &mut self,
        value: serde_json::Value,
    ) -> std::result::Result<Option<serde_json::Value>, String> {
        if value.get("type").and_then(|t| t.as_str()) != Some("rpc_chunk") {
            if !self.pending.is_empty() {
                // 对齐 omp：序列未交付前插入普通帧即视为打断；丢弃残片便于恢复。
                self.pending.clear();
                return Err("rpc chunk sequence interrupted".to_string());
            }
            return Ok(Some(value));
        }
        // 元数据校验（对齐 omp）：chunkId 非空且 ≤128 字节；count ≥ 2（分片仅用于
        // 超限帧）且 ≤ 重组上限/单片上限；index < count；byteLength 不低于单行上限、
        // 不超重组上限。
        let Some(chunk_id) = value.get("chunkId").and_then(|v| v.as_str()) else {
            return Err("invalid rpc chunk metadata".to_string());
        };
        if chunk_id.is_empty() || chunk_id.len() > 128 {
            return Err("invalid rpc chunk metadata".to_string());
        }
        let (Some(index), Some(count), Some(byte_length)) = (
            value.get("index").and_then(|v| v.as_u64()),
            value.get("count").and_then(|v| v.as_u64()),
            value.get("byteLength").and_then(|v| v.as_u64()),
        ) else {
            return Err("invalid rpc chunk metadata".to_string());
        };
        if count < 2
            || index >= count
            || count as usize > MAX_RPC_REASSEMBLED_BYTES.div_ceil(RPC_CHUNK_PAYLOAD_BYTES)
            || (byte_length as usize) < MAX_RPC_FRAME_BYTES
            || byte_length as usize > MAX_RPC_REASSEMBLED_BYTES
        {
            return Err("invalid rpc chunk metadata".to_string());
        }
        let Some(data) = value.get("data").and_then(|v| v.as_str()) else {
            return Err("invalid rpc chunk metadata".to_string());
        };
        // 严格 base64：解码失败 / 非规范编码 / 超单片上限均拒绝（对齐 omp decodeBase64）。
        let bytes = BASE64_STANDARD
            .decode(data)
            .ok()
            .filter(|decoded| {
                BASE64_STANDARD.encode(decoded) == data && decoded.len() <= RPC_CHUNK_PAYLOAD_BYTES
            })
            .ok_or_else(|| "invalid rpc chunk data".to_string())?;

        // 同一 chunkId 的元数据必须前后一致（对齐 omp sequence mismatch）。
        if let Some(pending) = self.pending.get(chunk_id) {
            if pending.count != count as usize || pending.byte_length != byte_length as usize {
                self.pending.remove(chunk_id);
                return Err("rpc chunk sequence mismatch".to_string());
            }
        } else {
            if self.pending.len() >= MAX_PENDING_CHUNK_SEQUENCES {
                return Err("rpc chunk sequence limit exceeded".to_string());
            }
            self.pending.insert(
                chunk_id.to_string(),
                PendingChunks {
                    count: count as usize,
                    byte_length: byte_length as usize,
                    slots: vec![None; count as usize],
                    received: 0,
                    received_bytes: 0,
                },
            );
        }
        let pending = self.pending.get_mut(chunk_id).expect("分片序列已在册");
        let slot_index = index as usize;
        if pending.slots[slot_index].is_some() {
            // 同一 (chunkId, index) 重复帧：幂等忽略，不重复计入。
            return Ok(None);
        }
        pending.received_bytes += bytes.len();
        pending.slots[slot_index] = Some(bytes);
        pending.received += 1;
        if pending.received_bytes > pending.byte_length {
            self.pending.remove(chunk_id);
            return Err("rpc chunk sequence exceeds declared length".to_string());
        }
        if pending.received < pending.count {
            return Ok(None);
        }
        // 集齐：按 index 顺序拼接（received == count 且槽位唯一 ⟹ 全部就位）。
        let pending = self.pending.remove(chunk_id).expect("分片序列已在册");
        let mut buf = Vec::with_capacity(pending.byte_length);
        for slot in &pending.slots {
            buf.extend_from_slice(slot.as_deref().expect("集齐时槽位必然齐全"));
        }
        if buf.len() != pending.byte_length {
            return Err("rpc chunk sequence length mismatch".to_string());
        }
        let text = std::str::from_utf8(&buf)
            .map_err(|_| "rpc chunk sequence produced invalid utf-8".to_string())?;
        let frame: serde_json::Value = serde_json::from_str(text)
            .map_err(|_| "rpc chunk sequence produced invalid json".to_string())?;
        if !frame.is_object() {
            return Err("rpc frame must be an object".to_string());
        }
        Ok(Some(frame))
    }
}

/// 解析一行入站 NDJSON 并推进重组器：普通帧直通返回；`rpc_chunk` 缓存 / 交付；
/// 协议错误返回错误文案（调用方回 `error` 行，与无效 JSON 同通道）。
fn decode_inbound_line(
    decoder: &mut RpcFrameDecoder,
    line: &str,
) -> std::result::Result<Option<serde_json::Value>, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|_| "invalid json request".to_string())?;
    decoder.push(value)
}

// ──────────────────────────────────────────────────────────────────────────────
// 会话控制（get_state / set_model / set_thinking / get_messages / compact / get_usage）
// ──────────────────────────────────────────────────────────────────────────────

/// `get_messages` 未指定 `limit` 时的默认条数。
const DEFAULT_MESSAGES_LIMIT: u64 = 50;
/// `get_messages` 单次返回的条数上限（防御超大方 payload）。
const MAX_MESSAGES_LIMIT: u64 = 10_000;
/// `get_messages` 单条消息文本预览的最大字符数（超出截断并追加省略号）。
const MESSAGE_PREVIEW_CHARS: usize = 200;

/// 运行时模型状态（`apply_model_switch` 的落点）：空闲主循环写（set_model /
/// prompt.model），turn 循环经 get_state 只读。
#[derive(Clone)]
struct ModelRuntime {
    api_key: String,
    base_url: String,
    max_output: usize,
    model: agent_core::Model,
    provider_ctx: agent_core::ProviderCallContext,
}

/// `set_thinking` 的共享思考预算（对齐 oh-my-pi `getReasoning` / `getDisableReasoning`）：
/// 经 [`agent::AgentBuilder::runtime_overrides`] 注入引擎，每轮 LLM 请求前解析——
/// 运行期调整即时生效，无需重建 Agent。
struct RpcThinkingOverrides {
    /// `None` = 未覆盖（沿用配置 / policy）；`Some(None)` = 显式关闭思考；
    /// `Some(Some(n))` = 预算 n token。三态语义有意为之，定向豁免 option_option。
    #[allow(clippy::option_option)]
    budget: std::sync::Mutex<Option<Option<usize>>>,
}

impl RpcThinkingOverrides {
    /// 构造未覆盖状态。
    fn new() -> Self {
        Self {
            budget: std::sync::Mutex::new(None),
        }
    }

    /// 当前生效预算快照（get_state 报告用；`None` = 未覆盖或已关闭）。
    fn snapshot(&self) -> Option<usize> {
        self.budget.lock().expect("thinking 预算锁").flatten()
    }
}

impl agent::RuntimeOverrides for RpcThinkingOverrides {
    fn thinking(&self, _model: &agent_core::Model) -> Option<agent_core::ThinkingConfig> {
        match &*self.budget.lock().expect("thinking 预算锁") {
            Some(Some(n)) => Some(agent_core::ThinkingConfig::new(*n)),
            _ => None,
        }
    }

    fn disable_thinking(&self) -> Option<bool> {
        matches!(&*self.budget.lock().expect("thinking 预算锁"), Some(None)).then_some(true)
    }
}

/// 会话控制共享状态：空闲主循环与运行中 turn 共用（get_state / set_thinking /
/// 轮次计数 / 流式标记；set_model 的模型状态仅空闲主循环写）。
struct SessionControl {
    /// 智能体模式（会话期恒定）。
    mode: agent_core::Mode,
    /// 工作目录（`get_available_commands` 发现项目级自定义命令用）。
    cwd: PathBuf,
    /// 共享 todo 清单状态（`set_todos` / `get_todos` 与 `todo` 工具同一实例）。
    todo: Arc<agent_tools::TodoState>,
    /// 共享 checkpoint 状态（`todo` 之外的会话内工具与 RPC 面共享）。
    #[allow(dead_code)]
    checkpoints: Arc<agent_tools::CheckpointState>,
    /// 会话 id。
    session_id: String,
    /// 会话落盘路径。
    session_file: PathBuf,
    /// 当前模型别名（prompt.model / set_model 成功后记录；`None` = 配置默认链）。
    alias: std::sync::Mutex<Option<String>>,
    /// 会话累计轮次（TurnStart 事件驱动，跨 prompt 请求累加）。
    turns: AtomicU64,
    /// 是否有 turn 正在运行。
    streaming: AtomicBool,
    /// 是否有 compact 任务在执行（空闲互斥；完成通知另行入队）。
    compacting: Arc<AtomicBool>,
    /// 思考预算覆盖（set_thinking 写，引擎每轮解析）。
    thinking: Arc<RpcThinkingOverrides>,
    /// 运行时模型状态。
    model: std::sync::Mutex<ModelRuntime>,
}

/// compact 完成通知（后台压缩任务 → 主循环单写者回帧）。
struct CompactNotice {
    /// 触发 compact 的请求 id（H9：任意 JSON 值原样回带）。
    id: serde_json::Value,
    /// 完成事件负载（`{"kind":"compact_done","tokens":{…}}`）。
    event: serde_json::Value,
}

/// 会话控制命令的处理结果。
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Reply 携带完整响应帧为常态路径；装箱 14 处构造点不值当。
enum ControlOutcome {
    /// 已生成响应帧，调用方直接写出。
    Reply(RpcEnvelope),
    /// set_model 成功：运行时模型状态已更新，调用方重建 Agent 后回 ok 帧。
    ModelSwitched {
        /// 切换后的模型 id（ok 帧 `model` 字段）。
        model_id: String,
    },
    /// compact 受理（仅空闲）：调用方回 ok 帧并启动后台压缩任务。
    CompactionStarted,
}

/// 是否为会话控制命令类型（prompt / cancel / ping 之外）。
fn is_session_command(ty: &str) -> bool {
    matches!(
        ty,
        "get_state"
            | "set_model"
            | "set_thinking"
            | "get_messages"
            | "compact"
            | "get_usage"
            | "get_tree"
            | "switch_branch"
            | "list_models"
            | "get_session_stats"
            | "get_available_commands"
            | "set_todos"
            | "get_todos"
            | "get_available_models"
            | "get_last_assistant_text"
    )
}

/// 解析 `set_todos` 负载 → Gyre 条目列表（H7）。
///
/// 支持两种形状：
/// - omp：`{phases:[{name, tasks:[{content, status, blocker?}]}]}`（分组名丢弃，返回 note）；
/// - Gyre：`{items:[{content, phase, blocked_reason?}]}`。
///
/// `status` / `phase` 取值同集：`pending` / `in_progress` / `completed` / `abandoned` / `blocked`。
fn todo_items_from_payload(
    payload: &serde_json::Value,
) -> Result<(Vec<agent_tools::TodoItem>, Option<String>), String> {
    let parse_status = |v: Option<&serde_json::Value>| -> Result<agent_tools::TodoPhase, String> {
        let raw = v.and_then(serde_json::Value::as_str).unwrap_or("pending");
        serde_json::from_value::<agent_tools::TodoPhase>(serde_json::Value::String(raw.into()))
            .map_err(|_| format!("invalid todo status: {raw:?}"))
    };
    let mut items: Vec<agent_tools::TodoItem> = Vec::new();
    if let Some(phases) = payload.get("phases").and_then(serde_json::Value::as_array) {
        for phase in phases {
            let tasks = phase
                .get("tasks")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            for task in tasks {
                let Some(content) = task.get("content").and_then(serde_json::Value::as_str) else {
                    return Err("todo task requires a string `content`".into());
                };
                items.push(agent_tools::TodoItem {
                    id: String::new(), // replace() 重排
                    content: content.to_string(),
                    phase: parse_status(task.get("status"))?,
                    blocked_reason: task
                        .get("blocker")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                });
            }
        }
        let grouped = payload
            .get("phases")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|p| !p.is_empty());
        return Ok((
            items,
            grouped.then(|| "omp 阶段分组名在 Gyre 无对应概念，已展平为条目".to_string()),
        ));
    }
    let Some(raw_items) = payload.get("items").and_then(serde_json::Value::as_array) else {
        return Err("set_todos expects `phases` (omp) or `items` (gyre)".into());
    };
    for item in raw_items {
        let Some(content) = item.get("content").and_then(serde_json::Value::as_str) else {
            return Err("todo item requires a string `content`".into());
        };
        items.push(agent_tools::TodoItem {
            id: String::new(),
            content: content.to_string(),
            phase: parse_status(item.get("phase").or_else(|| item.get("status")))?,
            blocked_reason: item
                .get("blocked_reason")
                .or_else(|| item.get("blocker"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        });
    }
    Ok((items, None))
}

/// 清单 → 线协议 JSON（与 omp `todoPhases` 字段名对齐用 `items`，另附 Markdown 渲染）。
fn todo_list_json(list: &agent_tools::TodoList) -> serde_json::Value {
    serde_json::json!({
        "items": list.items,
        "nextId": list.next_id,
    })
}

/// H7：把 `steer` / `follow_up` 文本投递到 steer 队列。
///
/// 空文本或队列不可用（Agent 未启用 steering）时返回错误文案；成功即表示已入队
/// （下一个 step 边界注入，**不**保证立刻生效）。
fn steer_running_turn(
    agent: &agent::Agent,
    text: Option<&str>,
    command: &str,
) -> Result<(), String> {
    let text = text.unwrap_or_default().trim();
    if text.is_empty() {
        return Err(format!("{command} requires a non-empty message"));
    }
    if agent.steer(AgentMessage::user_text(text.to_string())) {
        Ok(())
    } else {
        Err("steering queue unavailable (agent built without steering)".to_string())
    }
}

/// 已识别但 Gyre **未实现**的 omp RPC 命令（H7）。
///
/// 这些命令用 omp 信封回 `{type:"response",command,success:false,error}` —— 官方客户端
/// 能据此**降级**（而不是把 `unknown rpc message type` 当作协议错误/拼写错误）。
/// 语义上是「显式不受理」，不计入已实现命令面：
/// - 需要 Gyre 尚无的能力：宿主工具/URI 子协议、子代理订阅帧、扩展 UI、会话导出、
///   OAuth 经 RPC 登录、bash 会话内执行、快速模式、自动重试/压缩开关；
/// - 或仅有命名差异、待后续对齐：`set_thinking_level` / `cycle_thinking_level` /
///   `set_steering_mode` / `set_follow_up_mode` / `set_interrupt_mode` /
///   `switch_session` / `branch` / `get_branch_messages` / `get_messages_page` /
///   `set_session_name` / `handoff` / `new_session` / `cycle_model` /
///   `set_auto_compaction` / `set_auto_retry` / `abort_retry` / `abort_bash` /
///   `abort_and_prompt`。
const OMP_RECOGNIZED_UNSUPPORTED: &[&str] = &[
    "abort_and_prompt",
    "abort_bash",
    "abort_retry",
    "bash",
    "branch",
    "cycle_model",
    "cycle_thinking_level",
    "export_html",
    "get_branch_messages",
    "get_login_providers",
    "get_messages_page",
    "get_subagent_messages",
    "get_subagents",
    "handoff",
    "login",
    "new_session",
    "set_auto_compaction",
    "set_auto_retry",
    "set_fast_mode",
    "set_follow_up_mode",
    "set_host_tools",
    "set_host_uri_schemes",
    "set_interrupt_mode",
    "set_session_name",
    "set_steering_mode",
    "set_subagent_subscription",
    "set_thinking_level",
    "switch_session",
];

/// 若 `ty` 是「已识别但不支持」的 omp 命令 → 返回 omp 失败帧；否则 `None`。
fn unsupported_omp_command(id: serde_json::Value, ty: &str) -> Option<OmpResponseFrame> {
    OMP_RECOGNIZED_UNSUPPORTED.contains(&ty).then(|| {
        OmpResponseFrame::failure(
            id,
            ty,
            format!("{ty} 已识别但 Gyre 未实现（见 docs/oh-my-pi-gap-analysis-2026-09-11.md H7）"),
        )
    })
}

/// 模式的线协议名（serde lowercase 对齐）。
const fn mode_name(mode: agent_core::Mode) -> &'static str {
    match mode {
        agent_core::Mode::Code => "code",
        agent_core::Mode::Architect => "architect",
        agent_core::Mode::Ask => "ask",
        agent_core::Mode::Debug => "debug",
        agent_core::Mode::Plan => "plan",
    }
}

/// 解析 set_thinking 的 `budget` 参数：数字 → `Some(Some(n))`，`null` → `Some(None)`；
/// 缺失 / 类型非法 → 错误文案（调用方按 id 回错误帧）。
fn parse_budget(value: &Option<serde_json::Value>) -> Result<Option<usize>, String> {
    match value {
        None => Err("set_thinking requires budget (number or null)".to_string()),
        Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| "budget must be a non-negative integer or null".to_string()),
    }
}

/// 解析 get_messages 的 `limit` 参数：正整数（钳到 [`MAX_MESSAGES_LIMIT`]）；
/// 缺失 → [`DEFAULT_MESSAGES_LIMIT`]；类型非法 → 错误文案。
fn parse_limit(value: &Option<serde_json::Value>) -> Result<u64, String> {
    match value {
        None => Ok(DEFAULT_MESSAGES_LIMIT),
        Some(v) => v
            .as_u64()
            .map(|n| n.min(MAX_MESSAGES_LIMIT))
            .filter(|n| *n > 0)
            .ok_or_else(|| "limit must be a positive integer".to_string()),
    }
}

/// 截断文本到预览长度（字符边界安全；截断时追加省略号）。
fn truncate_preview(text: &str) -> String {
    if text.chars().count() <= MESSAGE_PREVIEW_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MESSAGE_PREVIEW_CHARS).collect();
    cut.push('…');
    cut
}

/// 把内部富消息压缩为 `{"role":…,"text":…}` 预览（get_messages 用）：
/// assistant 纯工具调用轮以 `[tool_call: …]` 标注，图像等非文本块不进预览。
fn message_preview(msg: &AgentMessage) -> serde_json::Value {
    let (role, text) = match msg {
        AgentMessage::User(u) => (
            "user",
            u.content
                .iter()
                .filter_map(|c| match c {
                    agent_core::UserContent::Text { text } => Some(text.as_str()),
                    agent_core::UserContent::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        ),
        AgentMessage::Assistant(a) => {
            let text = a.text();
            let text = if text.is_empty() && a.has_tool_calls() {
                let names: Vec<&str> = a.tool_calls().iter().map(|(_, name, _)| *name).collect();
                format!("[tool_call: {}]", names.join(", "))
            } else {
                text
            };
            ("assistant", text)
        }
        AgentMessage::ToolResult(t) => ("tool_result", t.result.to_llm_text()),
        AgentMessage::Status(s) => ("status", s.text.clone()),
        AgentMessage::Ask(a) => ("ask", a.prompt.clone()),
        AgentMessage::SoftRequirement(r) => ("soft_requirement", r.reminder.clone()),
    };
    serde_json::json!({ "role": role, "text": truncate_preview(&text) })
}

/// 取活跃路径（根→叶）的消息快照；不支持树语义的上下文退回全量节点快照。
async fn active_path_messages(context: &dyn agent_core::ContextManager) -> Vec<AgentMessage> {
    let nodes = context.snapshot_nodes().await;
    if nodes.is_empty() {
        return Vec::new();
    }
    let Some(leaf) = context.active_leaf().await else {
        return nodes.into_iter().map(|n| n.message).collect();
    };
    let by_id: HashMap<&str, &agent_core::SessionNode> =
        nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut chain: Vec<&AgentMessage> = Vec::new();
    let mut cursor = Some(leaf);
    while let Some(id) = cursor {
        let Some(node) = by_id.get(id.as_str()) else {
            break;
        };
        chain.push(&node.message);
        cursor = node.parent_id.clone();
        // 防御异常持久化数据成环：链长超过节点总数即止。
        if chain.len() > nodes.len() {
            break;
        }
    }
    chain.reverse();
    chain.into_iter().cloned().collect()
}

/// 处理一个会话控制请求（无 I/O；compact 的后台任务由调用方启动）。
async fn dispatch_session_command(
    req: &RpcRequest,
    control: &SessionControl,
    context: &dyn agent_core::ContextManager,
    cfg: &agent_config::Config,
    oauth_client: &reqwest::Client,
    config_dir: &std::path::Path,
) -> ControlOutcome {
    match req.ty.as_str() {
        "get_state" => {
            let alias = control.alias.lock().expect("模型别名锁").clone();
            let model = control.model.lock().expect("模型运行时锁").model.clone();
            let usage = context.token_usage();
            let message_count = active_path_messages(context).await.len();
            let state = serde_json::json!({
                "model": {
                    "id": model.id,
                    "provider": model.provider,
                    "maxInputTokens": model.max_input_tokens,
                    "maxOutputTokens": model.max_output_tokens,
                },
                "modelAlias": alias,
                "mode": mode_name(control.mode),
                "sessionId": control.session_id,
                "sessionFile": control.session_file.display().to_string(),
                "contextTokens": usage.current,
                "contextLimit": usage.limit,
                "turns": control.turns.load(Ordering::Relaxed),
                "messageCount": message_count,
                "thinkingBudget": control.thinking.snapshot(),
                "isStreaming": control.streaming.load(Ordering::Relaxed),
                "isCompacting": control.compacting.load(Ordering::Relaxed),
            });
            ControlOutcome::Reply(RpcEnvelope::state(RpcEnvelope::req_id(req), state))
        }
        "get_session_stats" => {
            // H7：omp `getSessionStats()` 形状的子集（会话身份 + 消息/工具计数 + 用量 + 上下文）。
            let messages = active_path_messages(context).await;
            let mut user_messages = 0usize;
            let mut assistant_messages = 0usize;
            let mut tool_results = 0usize;
            let mut tool_calls = 0usize;
            for m in &messages {
                match m {
                    AgentMessage::User(_) => user_messages += 1,
                    AgentMessage::Assistant(a) => {
                        assistant_messages += 1;
                        tool_calls += a.tool_calls().len();
                    }
                    AgentMessage::ToolResult(_) => tool_results += 1,
                    _ => {}
                }
            }
            let usage = context.token_usage();
            let stats = serde_json::json!({
                "sessionId": control.session_id,
                "sessionFile": control.session_file.display().to_string(),
                "mode": mode_name(control.mode),
                "turns": control.turns.load(Ordering::Relaxed),
                "userMessages": user_messages,
                "assistantMessages": assistant_messages,
                "toolCalls": tool_calls,
                "toolResults": tool_results,
                "totalMessages": messages.len(),
                "contextUsage": {
                    "current": usage.current,
                    "limit": usage.limit,
                },
                "isStreaming": control.streaming.load(Ordering::Relaxed),
            });
            ControlOutcome::Reply(RpcEnvelope::state(RpcEnvelope::req_id(req), stats))
        }
        "get_available_commands" => {
            // H7：可用斜杠命令清单（omp `get_available_commands` →
            // `{commands:[{name, description, source}]}`）。omp 的 `name` **不带**前导 `/`
            // （客户端自行补），故此处统一剥离；`source` 区分 builtin / file / mcp。
            let custom = agent_config::discover_commands(&control.cwd);
            let builtins: std::collections::HashSet<String> =
                crate::repl::all_command_names_with_prompts(&[], &[])
                    .into_iter()
                    .collect();
            let file_names: std::collections::HashSet<String> =
                custom.iter().map(|c| format!("/{}", c.name)).collect();
            let commands: Vec<serde_json::Value> =
                crate::repl::all_command_names_with_prompts(&custom, &[])
                    .into_iter()
                    .map(|full| {
                        let source = if builtins.contains(&full) {
                            "builtin"
                        } else if file_names.contains(&full) {
                            "file"
                        } else {
                            "mcp"
                        };
                        serde_json::json!({
                            "name": full.trim_start_matches('/'),
                            "description": null,
                            "source": source,
                        })
                    })
                    .collect();
            ControlOutcome::Reply(RpcEnvelope::state(
                RpcEnvelope::req_id(req),
                serde_json::json!({ "commands": commands }),
            ))
        }
        "set_todos" => {
            // H7：omp `set_todos`（`{phases:[{name,tasks:[{content,status,blocker}]}]}`）与
            // Gyre 原生形态（`{items:[…]}`）双收。Gyre 的 `TodoPhase` 是**状态**枚举
            // （pending/in_progress/completed/abandoned/blocked，即 omp 的 `TodoStatus`），
            // 没有 omp 的阶段分组概念 → 分组名丢弃（记入返回值的 note）。
            let Some(payload) = req.todos.as_ref() else {
                return ControlOutcome::Reply(RpcEnvelope::error(
                    RpcEnvelope::req_id(req),
                    "set_todos requires `phases` (omp) or `items` (gyre)",
                ));
            };
            let (items, note) = match todo_items_from_payload(payload) {
                Ok(v) => v,
                Err(msg) => {
                    return ControlOutcome::Reply(RpcEnvelope::error(
                        RpcEnvelope::req_id(req),
                        msg,
                    ));
                }
            };
            let list = control.todo.replace(items);
            let mut value = todo_list_json(&list);
            if let Some(note) = note {
                value["note"] = serde_json::Value::String(note);
            }
            ControlOutcome::Reply(RpcEnvelope::state(RpcEnvelope::req_id(req), value))
        }
        "get_available_models" => {
            // H7：omp `get_available_models` → `{models:[Model]}`。数据源是**配置的**
            // profile（不是网络发现；网络发现是 Gyre 的 `list_models`）。
            let mut models = vec![serde_json::json!({
                "id": cfg.default_model.id,
                "alias": serde_json::Value::Null,
                "provider": cfg.default_model.api.as_str(),
                "isDefault": true,
            })];
            for m in &cfg.models {
                models.push(serde_json::json!({
                    "id": m.id,
                    "alias": m.alias,
                    "provider": m.api.as_str(),
                    "isDefault": false,
                }));
            }
            ControlOutcome::Reply(RpcEnvelope::state(
                RpcEnvelope::req_id(req),
                serde_json::json!({ "models": models }),
            ))
        }
        "get_last_assistant_text" => {
            // H7：最后一条 assistant 消息的纯文本（omp 同名命令）。
            let messages = active_path_messages(context).await;
            let text = messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    AgentMessage::Assistant(a) => {
                        let t = a.text();
                        (!t.is_empty()).then_some(t)
                    }
                    _ => None,
                })
                .unwrap_or_default();
            ControlOutcome::Reply(RpcEnvelope::state(
                RpcEnvelope::req_id(req),
                serde_json::json!({ "text": text }),
            ))
        }
        "get_todos" => {
            let list = control.todo.snapshot();
            ControlOutcome::Reply(RpcEnvelope::state(
                RpcEnvelope::req_id(req),
                todo_list_json(&list),
            ))
        }
        "set_model" => {
            let Some(alias) = req.alias.as_deref().filter(|a| !a.is_empty()) else {
                return ControlOutcome::Reply(RpcEnvelope::error(
                    RpcEnvelope::req_id(req),
                    "set_model requires alias",
                ));
            };
            let (switched, model_id) = {
                let mut rt = control.model.lock().expect("模型运行时锁");
                // MutexGuard 的 DerefMut 每次字段访问都是一次新的独占借用，
                // 先落一次 &mut *rt 再按不相交字段借用，才能多字段同调用。
                let m = &mut *rt;
                let switched = apply_model_switch(
                    alias,
                    cfg,
                    oauth_client,
                    config_dir,
                    &mut m.api_key,
                    &mut m.base_url,
                    &mut m.max_output,
                    &mut m.model,
                    &mut m.provider_ctx,
                );
                (switched, m.model.id.clone())
            };
            if !switched {
                return ControlOutcome::Reply(RpcEnvelope::error(
                    RpcEnvelope::req_id(req),
                    format!("model switch failed: {alias}"),
                ));
            }
            *control.alias.lock().expect("模型别名锁") = Some(alias.to_string());
            ControlOutcome::ModelSwitched { model_id }
        }
        "set_thinking" => match parse_budget(&req.budget) {
            Err(message) => {
                ControlOutcome::Reply(RpcEnvelope::error(RpcEnvelope::req_id(req), message))
            }
            Ok(budget) => {
                *control.thinking.budget.lock().expect("thinking 预算锁") = Some(budget);
                ControlOutcome::Reply(RpcEnvelope::ok_budget(RpcEnvelope::req_id(req), budget))
            }
        },
        "get_messages" => {
            let limit = match parse_limit(&req.limit) {
                Ok(limit) => limit as usize,
                Err(message) => {
                    return ControlOutcome::Reply(RpcEnvelope::error(
                        RpcEnvelope::req_id(req),
                        message,
                    ));
                }
            };
            let messages = active_path_messages(context).await;
            let start = messages.len().saturating_sub(limit);
            let previews: Vec<serde_json::Value> =
                messages[start..].iter().map(message_preview).collect();
            ControlOutcome::Reply(RpcEnvelope::messages(
                RpcEnvelope::req_id(req),
                serde_json::Value::Array(previews),
            ))
        }
        "compact" => ControlOutcome::CompactionStarted,
        "get_usage" => ControlOutcome::Reply(RpcEnvelope::usage(
            RpcEnvelope::req_id(req),
            usage_json(&context.accumulated_usage()),
        )),
        "get_tree" => {
            let nodes = context.snapshot_nodes().await;
            let active = context.active_leaf().await;
            ControlOutcome::Reply(RpcEnvelope::tree(
                RpcEnvelope::req_id(req),
                agent_cli::tree_ui::tree_json(&nodes, active.as_deref()),
            ))
        }
        "switch_branch" => {
            let nodes = context.snapshot_nodes().await;
            let Some(input) = req.node.as_deref().filter(|s| !s.is_empty()) else {
                return ControlOutcome::Reply(RpcEnvelope::error(
                    RpcEnvelope::req_id(req),
                    "switch_branch requires node",
                ));
            };
            let Some(node) = agent_cli::tree_ui::parse_node_target(input, &nodes) else {
                return ControlOutcome::Reply(RpcEnvelope::error(
                    RpcEnvelope::req_id(req),
                    format!("node not found or ambiguous: {input}"),
                ));
            };
            let handoff = req.handoff.unwrap_or(false);
            let switched = if handoff {
                match context.switch_branch_with_handoff(&node).await {
                    Ok(v) => v,
                    Err(e) => {
                        return ControlOutcome::Reply(RpcEnvelope::error(
                            RpcEnvelope::req_id(req),
                            format!("handoff 切换失败: {e}"),
                        ));
                    }
                }
            } else {
                context.set_active_leaf(&node).await
            };
            if !switched {
                return ControlOutcome::Reply(RpcEnvelope::error(
                    RpcEnvelope::req_id(req),
                    if handoff {
                        "handoff 不可用（该上下文无摘要器）或节点不存在".to_string()
                    } else {
                        format!("节点不存在: {node}")
                    },
                ));
            }
            ControlOutcome::Reply(RpcEnvelope::ok(RpcEnvelope::req_id(req)))
        }
        "list_models" => {
            // 运行时发现：当前运行时模型的 api/base_url/key（set_model 切换后即发现新源）。
            let (api, base_url, api_key) = {
                let m = control.model.lock().expect("模型运行时锁");
                (m.model.api, m.base_url.clone(), m.api_key.clone())
            };
            let http = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| anyhow::anyhow!("HTTP 客户端构建失败: {e}"))
                .unwrap_or_default();
            match agent_llm::list_models(api, &base_url, &api_key, http).await {
                Ok(list) => {
                    let models: Vec<serde_json::Value> = list
                        .iter()
                        .map(|dm| {
                            serde_json::json!({
                                "id": dm.id,
                                "ownedBy": dm.owned_by,
                            })
                        })
                        .collect();
                    ControlOutcome::Reply(RpcEnvelope::models_list(
                        RpcEnvelope::req_id(req),
                        serde_json::Value::Array(models),
                    ))
                }
                Err(e) => ControlOutcome::Reply(RpcEnvelope::error(
                    RpcEnvelope::req_id(req),
                    format!("模型发现失败: {e}"),
                )),
            }
        }
        _ => ControlOutcome::Reply(RpcEnvelope::error(
            RpcEnvelope::req_id(req),
            "unknown rpc message type",
        )),
    }
}

/// 执行压缩序列（shake → summarize → prune，与 main() `/compact` 一致）并发送完成通知。
/// 由 [`run_rpc`] 在空闲受理后经 `tokio::spawn` 调度；单写者约束下通知经通道回主循环回帧。
async fn run_compaction(
    context: Arc<dyn agent_core::ContextManager>,
    control: Arc<SessionControl>,
    id: serde_json::Value,
    tx: tokio::sync::mpsc::Sender<CompactNotice>,
) {
    let _ = context.compact(CompactionStrategy::Shake).await;
    let _ = context
        .compact(CompactionStrategy::Summarize { max_tokens: 0 })
        .await;
    let _ = context
        .compact(CompactionStrategy::Prune { keep_recent: 8 })
        .await;
    let usage = context.token_usage();
    control.compacting.store(false, Ordering::Relaxed);
    eprintln!(
        "{}",
        t!("compact.done", current = usage.current, limit = usage.limit)
    );
    let _ = tx
        .send(CompactNotice {
            id,
            event: serde_json::json!({
                "kind": "compact_done",
                "tokens": { "current": usage.current, "limit": usage.limit },
            }),
        })
        .await;
}

// ──────────────────────────────────────────────────────────────────────────────
// 驱动层（stdin / stdout 行循环）
// ──────────────────────────────────────────────────────────────────────────────

/// 共享 stdin 协议通道（主循环与运行中 turn 共用同一缓冲，保证多字节行不被拆分）。
type StdinReader = Arc<tokio::sync::Mutex<tokio::io::BufReader<tokio::io::Stdin>>>;

/// 从共享协议通道读一行（自动清空缓冲）。
async fn read_line(reader: &StdinReader, buf: &mut String) -> std::io::Result<usize> {
    let mut r = reader.lock().await;
    buf.clear();
    r.read_line(buf).await
}

/// 写一个逻辑帧并立即 flush（客户端实时可见）：未超限单行直通，超限自动 `rpc_chunk` 分片。
async fn write_frame<W: tokio::io::AsyncWrite + Unpin, T: serde::Serialize>(
    out: &mut W,
    env: &T,
) -> Result<()> {
    let json = encode_json(env)?;
    for line in encode_frame_lines(&json) {
        out.write_all(line.as_bytes()).await?;
    }
    out.flush().await?;
    Ok(())
}

/// 转发一个 agent 事件为 RPC event 行，并更新 turn 状态（用量 / 轮数 / 成败 / 错误）。
#[allow(clippy::too_many_arguments)] // 事件转发面天然多参，同 main.rs::run_swarm 先例。
async fn handle_turn_event(
    ev: &AgentEvent,
    id: serde_json::Value,
    out: &mut tokio::io::BufWriter<tokio::io::Stdout>,
    usage: &mut Usage,
    turns: &mut u64,
    success: &mut Option<bool>,
    last_error: &mut Option<String>,
    session_turns: &AtomicU64,
) -> Result<()> {
    write_agent_event(out, &id, ev).await?;
    match ev {
        AgentEvent::Usage(u) => usage.add(u),
        AgentEvent::TurnStart => {
            *turns += 1;
            // 会话累计轮次（get_state 的 `turns` 字段，跨 prompt 请求累加）。
            session_turns.fetch_add(1, Ordering::Relaxed);
        }
        AgentEvent::Done(summary) => {
            *success = Some(summary.success);
            *usage = summary.usage.clone();
            *turns = summary.turns;
        }
        AgentEvent::Error(e) => *last_error = Some(e.clone()),
        _ => {}
    }
    Ok(())
}

/// 运行一个 prompt 请求对应的完整 turn：事件流式转发为 event 行，最终以 done 行收尾
/// （任何路径——正常结束 / 取消 / 流中断——都保证恰好一条 done）。
///
/// 运行期间并发读 stdin：`cancel` 取消当前轮、`ping` / 查询类会话控制命令即时应答、
/// 重复 `prompt` / 变更型命令（set_model / compact）回 busy 错误。
/// 返回 `Ok(true)` 表示进程应退出（stdin 关闭或收到 SIGINT）；`Ok(false)` 继续服务。
#[allow(clippy::too_many_arguments)] // 会话控制状态注入面，同 main.rs 先例。
async fn run_turn_rpc(
    agent: &agent::Agent,
    id: serde_json::Value,
    text: &str,
    stdin_reader: &StdinReader,
    out: &mut tokio::io::BufWriter<tokio::io::Stdout>,
    decoder: &mut RpcFrameDecoder,
    sigint: &mut tokio::sync::mpsc::Receiver<()>,
    control: &SessionControl,
    context: &Arc<dyn agent_core::ContextManager>,
    cfg: &agent_config::Config,
    oauth_client: &reqwest::Client,
    config_dir: &std::path::Path,
    reverse: &ReverseChannel,
    reverse_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) -> Result<bool> {
    control.streaming.store(true, Ordering::Relaxed);
    // 每轮独立取消作用域：cancel 请求 / SIGINT 命中即中断流式与在途工具，Agent 可继续下一轮。
    let cancel = tokio_util::sync::CancellationToken::new();
    let events = agent.run_with_cancel(text, cancel.clone());
    tokio::pin!(events);

    let mut success: Option<bool> = None;
    let mut usage = Usage::default();
    let mut turns: u64 = 0;
    let mut cancelled = false;
    let mut last_error: Option<String> = None;
    let mut stdin_eof = false;
    let mut exit_after = false;
    let mut buf = String::new();

    loop {
        if stdin_eof {
            // 协议通道已关闭：不再并发读，专心排空事件流。
            match events.next().await {
                Some(ev) => {
                    handle_turn_event(
                        &ev,
                        id.clone(),
                        out,
                        &mut usage,
                        &mut turns,
                        &mut success,
                        &mut last_error,
                        &control.turns,
                    )
                    .await?;
                }
                None => break,
            }
            continue;
        }
        tokio::select! {
            biased;
            ev = events.next() => match ev {
                Some(ev) => {
                    handle_turn_event(
                        &ev,
                        id.clone(),
                        out,
                        &mut usage,
                        &mut turns,
                        &mut success,
                        &mut last_error,
                        &control.turns,
                    )
                    .await?;
                }
                None => break,
            },
            n = read_line(stdin_reader, &mut buf) => match n {
                Ok(0) => stdin_eof = true,
                Err(e) => return Err(e.into()),
                Ok(_) => {
                    if buf.trim().is_empty() {
                        continue;
                    }
                    let line = buf.trim().to_string();
                    match decode_inbound_line(decoder, &line) {
                        Err(msg) => {
                            write_frame(out, &RpcEnvelope::error(0, msg)).await?;
                        }
                        Ok(None) => {} // 分片未凑齐：无请求可处理。
                        Ok(Some(frame)) => match serde_json::from_value::<RpcRequest>(frame) {
                            Ok(r) if r.ty == "cancel" => {
                                cancel.cancel();
                                cancelled = true;
                            }
                            // H7：omp `steer` / `follow_up` —— 投递到运行中 turn 的 steering 队列，
                            // 在下一个 step 边界注入（omp 语义：steer 立即插话、follow_up 停止后处理；
                            // Gyre 的 steering 队列在 step 边界排空，二者共用同一通道）。
                            Ok(r) if r.ty == "steer" || r.ty == "follow_up" => {
                                let command = if r.ty == "steer" { "steer" } else { "follow_up" };
                                let frame = match steer_running_turn(agent, r.text.as_deref(), command) {
                                    Ok(()) => OmpResponseFrame::success(
                                        RpcEnvelope::req_id(&r),
                                        command,
                                        None,
                                    ),
                                    Err(msg) => {
                                        OmpResponseFrame::failure(RpcEnvelope::req_id(&r), command, msg)
                                    }
                                };
                                write_frame(out, &frame).await?;
                            }
                            // H7：omp `abort` —— `cancel` 的 omp 命名别名，附成功响应帧。
                            Ok(r) if r.ty == "abort" => {
                                cancel.cancel();
                                cancelled = true;
                                write_frame(
                                    out,
                                    &OmpResponseFrame::success(RpcEnvelope::req_id(&r), "abort", None),
                                )
                                .await?;
                            }
                            Ok(r) if r.ty == "ping" => {
                                write_frame(out, &RpcEnvelope::pong(r.id)).await?;
                            }
                            Ok(r) if r.ty == "response" => {
                                // 宿主回答 → 待答表；迟到 / 重复静默忽略。
                                let value = r
                                    .result
                                    .unwrap_or_else(|| serde_json::json!({ "answer": "no" }));
                                reverse.resolve(&r.id.clone().unwrap_or(serde_json::Value::Null), value);
                            }
                            Ok(r) if r.ty == "set_model" || r.ty == "compact" => {
                                // 变更型命令仅空闲受理：模型切换需重建 Agent，压缩与事件流互斥。
                                write_frame(
                                    out,
                                    &RpcEnvelope::error(
                                        r.id,
                                        "a turn is already running (send cancel first)",
                                    ),
                                )
                                .await?;
                            }
                            Ok(r) if is_session_command(&r.ty) => {
                                // 查询类（get_state / set_thinking / get_messages / get_usage）
                                // 运行中同样受理：读共享状态 / 上下文，无 Agent 参与。
                                let outcome = dispatch_session_command(
                                    &r,
                                    control,
                                    context.as_ref(),
                                    cfg,
                                    oauth_client,
                                    config_dir,
                                )
                                .await;
                                if let ControlOutcome::Reply(env) = outcome {
                                    write_frame(out, &env).await?;
                                }
                            }
                            Ok(r) if r.ty == "prompt" => {
                                write_frame(
                                    out,
                                    &RpcEnvelope::error(
                                        r.id,
                                        "a turn is already running (send cancel first)",
                                    ),
                                )
                                .await?;
                            }
                            Ok(r) => {
                                // H7：运行中收到「已识别但不支持」的 omp 命令同样回 omp 信封。
                                match unsupported_omp_command(RpcEnvelope::req_id(&r), &r.ty) {
                                    Some(frame) => write_frame(out, &frame).await?,
                                    None => {
                                        write_frame(
                                            out,
                                            &RpcEnvelope::error(
                                                RpcEnvelope::req_id(&r),
                                                "unknown rpc message type",
                                            ),
                                        )
                                        .await?;
                                    }
                                }
                            }
                            Err(_) => {
                                write_frame(out, &RpcEnvelope::error(0, "invalid json request"))
                                    .await?;
                            }
                        },
                    }
                }
            },
            _ = sigint.recv() => {
                // Ctrl-C：取消当前 turn，收尾后退出进程。
                cancel.cancel();
                cancelled = true;
                exit_after = true;
            }
            line = reverse_rx.recv() => {
                // 反向通道排空：审批 / 追问 request 行原样落 stdout。
                if let Some(line) = line {
                    write_frame(out, &line).await?;
                }
            }
        }
    }

    // 收尾帧：v2（omp）对端回 `{type:"response",command:"prompt",success,…}`——
    // omp 客户端等的是这个响应帧（事件已以 omp 形状裸帧外发，见 H8）；
    // v1 对端保持既有 `done` 行（docs/rpc.md 契约）。
    control.streaming.store(false, Ordering::Relaxed);
    if RPC_V2_NEGOTIATED.load(Ordering::Relaxed) {
        let frame = match success {
            Some(true) => OmpResponseFrame::success(
                id,
                "prompt",
                Some(serde_json::json!({
                    "usage": usage_json(&usage),
                    "turns": turns,
                })),
            ),
            _ => {
                let err = if cancelled {
                    "cancelled".to_string()
                } else {
                    last_error.unwrap_or_else(|| "turn failed".to_string())
                };
                OmpResponseFrame::failure(id, "prompt", err)
            }
        };
        write_frame(out, &frame).await?;
    } else {
        let env = match success {
            Some(s) if s => RpcEnvelope::done_ok(id, &usage, turns),
            Some(_) => {
                RpcEnvelope::done_err(id, last_error.unwrap_or_else(|| "turn failed".to_string()))
            }
            None => {
                let err = if cancelled {
                    "cancelled".to_string()
                } else {
                    last_error.unwrap_or_else(|| "stream ended unexpectedly".to_string())
                };
                RpcEnvelope::done_err(id, err)
            }
        };
        write_frame(out, &env).await?;
    }
    Ok(exit_after || stdin_eof)
}

// ── 反向通道（agent→host 请求 / 宿主回答回路由，`--rpc-forward-ask`）─────────────

/// 反向请求超时：宿主 10 分钟未回答按拒绝处理（对齐 ACP `PERMISSION_TIMEOUT_SECS`）。
const REVERSE_ASK_TIMEOUT_SECS: u64 = 600;

/// agent→host 反向请求通道：审批 / 追问不再自动拒绝，而是以
/// `{"type":"request","id":R,"method":"ask","params":{…}}` 行外发，宿主以
/// `{"type":"response","id":R,"result":{"answer":"yes"|"no","text":…}}` 回答
/// （error 帧 / 缺 result 按拒绝）。仅 `--rpc-forward-ask` 启用时装配；未启用保持
/// 「自动拒绝」兼容行为。反向 id 与宿主请求 id 命名空间独立（方向不同，无碰撞面）。
struct ReverseChannel {
    /// 出向 request 行队列（主循环 / turn 循环 select 臂排空写 stdout）。
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// 待宿主回答表：反向 id → 一次性回传端。
    pending: std::sync::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<serde_json::Value>>>,
    /// 反向 id 计数。
    next: AtomicU64,
}

impl ReverseChannel {
    fn new() -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(Self {
                tx,
                pending: std::sync::Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
            }),
            rx,
        )
    }

    /// 外发一个 ask 请求并等待宿主回答；超时 / 出向通道关闭 → 拒绝（宁拒勿挂）。
    async fn ask(&self, ask: &AskMessage) -> AskResponse {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let params = match &ask.kind {
            AskKind::Tool { tool } => {
                serde_json::json!({ "kind": "tool", "tool": tool, "prompt": ask.prompt })
            }
            AskKind::Command { command } => {
                serde_json::json!({ "kind": "command", "command": command, "prompt": ask.prompt })
            }
            other => serde_json::json!({
                "kind": ask_kind_tag(other),
                "prompt": ask.prompt,
            }),
        };
        let line = serde_json::json!({
            "type": "request",
            "id": id,
            "method": "ask",
            "params": params,
        });
        let Ok(json) = serde_json::to_string(&line) else {
            return AskResponse::No;
        };
        // 先登记待答表再发送：宿主可能在出向行被排空前就回答，
        // 发送后才登记会让早答落空（被当迟到丢弃）。
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        self.pending.lock().expect("反向请求表锁").insert(id, rtx);
        if self.tx.send(json).is_err() {
            self.pending.lock().expect("反向请求表锁").remove(&id);
            return AskResponse::No;
        }
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(REVERSE_ASK_TIMEOUT_SECS),
            rrx,
        )
        .await;
        self.pending.lock().expect("反向请求表锁").remove(&id);
        match answer {
            Ok(Ok(value)) => parse_host_answer(&value),
            _ => AskResponse::No, // 超时 / 发送端丢弃 / 应答器关闭
        }
    }

    /// 路由宿主回答：命中待答请求 → 投递并返回 true；未命中（迟到 / 重复）→ false。
    fn resolve(&self, id: &serde_json::Value, value: serde_json::Value) -> bool {
        let Some(id) = id.as_u64() else {
            return false; // 反向通道 id 为内部生成的数字；非数字 id 无从匹配
        };
        self.pending
            .lock()
            .expect("反向请求表锁")
            .remove(&id)
            .map(|tx| tx.send(value).is_ok())
            .unwrap_or(false)
    }
}

/// `AskKind` → `params.kind` 标签（snake_case，与线协议 serde 命名一致）。
fn ask_kind_tag(kind: &AskKind) -> &'static str {
    match kind {
        AskKind::Tool { .. } => "tool",
        AskKind::Command { .. } => "command",
        AskKind::Followup => "followup",
        AskKind::CompletionResult => "completion_result",
    }
}

/// 宿主回答 → `AskResponse`：`{"answer":"yes"}` / `{"answer":"no"}` /
/// `{"answer":"text","text":"…"}`；畸形 / 未知 answer → 拒绝（宁拒勿挂）。
fn parse_host_answer(value: &serde_json::Value) -> AskResponse {
    match value.get("answer").and_then(|a| a.as_str()) {
        Some("yes") => AskResponse::Yes,
        Some("text") => AskResponse::Text(
            value
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
        ),
        _ => AskResponse::No,
    }
}

/// 构造走反向通道的审批回调（`--rpc-forward-ask`）。
fn forward_ask_resolver(channel: Arc<ReverseChannel>) -> agent_config::PromptResolver {
    Arc::new(move |ask: AskMessage| {
        let channel = Arc::clone(&channel);
        Box::pin(async move { Ok(channel.ask(&ask).await) })
    })
}

/// 主入口：装配（镜像 `main()` 的单次任务路径）后进入 NDJSON 行循环。
pub async fn run_rpc(
    cfg: agent_config::Config,
    cwd: PathBuf,
    socks_override: Option<bool>,
    forward_ask: bool,
) -> Result<()> {
    // ── 装配：模型 profile（默认链）→ Provider → Tools / Context / Prompt ──
    let chain = cfg
        .resolve_chain(None)
        .context("解析默认模型 profile 失败")?;
    let profile = chain[0];

    // SOCKS5 代理控制器（仅影响后端出站请求；开关经 --socks / 配置 / sidecar）。
    let socks5 = super::build_socks5_controller(&cfg, &cwd, socks_override);
    super::print_socks5_status(&socks5);
    // 共享 HTTP 客户端：仅设连接超时 + keepalive，不设整条请求总超时（专供流式 LLM 调用）。
    // （构建顺序前移：OAuth「先刷后用」解析需要该客户端；克隆供 set_model 热切换复用。）
    let client = agent_proxy::build_http_client(socks5, cfg.user_agent.as_deref(), |b| {
        b.tcp_keepalive(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
    })
    .context("构建 HTTP 客户端失败")?;
    let oauth_client = client.clone();
    let rpc_config_dir = agent_core::platform::config_dir().unwrap_or_else(|| PathBuf::from("."));
    // 认证链分层（config 值含 ${ENV} 展开 → oauth.toml 有效/先刷后用 → auth.toml
    // → GYRE_<PROVIDER>_API_KEY env）。此前仅读 profile 内联值——auth.toml/env
    // 在 RPC 路径从未生效，本轮对齐交互路径。
    // H24：`auth = "none"` 不发内置鉴权头；`oauth` 只认 OAuth 存储（缺失即报错）。
    let api_key: Option<String> = agent_llm::oauth::resolve_profile_api_key(
        &oauth_client,
        &rpc_config_dir,
        profile.auth,
        profile.api.as_str(),
        &profile.id,
        profile.api_key.expose_secret().is_empty(),
        profile.resolve_api_key().expose_secret(),
    )
    .map_err(anyhow::Error::msg)?;
    let model = profile.to_model(cfg.agent.enable_thinking);
    let fallback_models: Vec<agent_core::Model> = chain
        .iter()
        .skip(1)
        .map(|p| p.to_model(cfg.agent.enable_thinking))
        .collect();
    let key_rings: std::collections::HashMap<String, Vec<String>> =
        chain.iter().map(|p| (p.id.clone(), p.key_ring())).collect();

    let mut registry = agent_llm::ProviderRegistry::new();
    for p in agent_llm::collect_providers(client) {
        registry.register(p);
    }
    let provider: Arc<dyn agent_core::LlmProvider> = agent_llm::wrap_inband_if(
        Arc::new(registry),
        std::env::var("GYRE_INBAND_TOOLS").ok().as_deref(),
    );
    let provider_ctx = agent_core::ProviderCallContext {
        api_key: api_key.clone(),
        base_url: Some(profile.effective_base_url()),
        max_in_flight: None,
        headers: crate::profile_headers(profile),
        quirks: profile.effective_quirks(),
        auth: profile.auth,
    };

    let mode = cfg.agent.mode;
    let prompts = Arc::new(agent_prompt::PromptCatalog::new());
    // 会话持久化：一个进程一个会话（Context 跨 prompt 请求复用，累积上下文）。
    let session_store = agent_context::SessionStore::for_cwd(&cwd);
    let session_id = agent_context::SessionStore::new_id();
    // H31：记录面包屑，使 `agent --continue` 能续上 RPC 会话。
    agent_context::SessionStore::for_cwd(&cwd).mark_last(&session_id);
    eprintln!("{}", t!("session.label", id = session_id));
    let session_path = session_store.path_for(&session_id);
    // H31：RPC 会话同样写 header 身份字段（cwd / prompt-cache key）。
    let mut session_header = agent_context::SessionHeader::new("gyre").with_path(&session_path);
    session_header.cwd = Some(cwd.display().to_string());
    session_header.provider_prompt_cache_key = Some(session_id.clone());
    let pctx = agent_context::PersistentContext::open_with_header(
        prompts.system_with_platform(mode),
        &session_path,
        session_header.clone(),
    )
    .await
    .context("打开持久化上下文失败")?;
    // 长期记忆（可选；按 cwd 项目作用域，backend 可切换）。
    // 提前到 summarizer 之前：压缩前召回（preCompactionContext）需把记忆挂到摘要提供器。
    let mut memory: Option<Arc<dyn agent_core::MemoryStore>> = None;
    let mut local_memory: Option<Arc<agent_memory::LocalMemoryStore>> = None;
    let mut structured_memory: Option<Arc<agent_memory::StructuredMemoryStore>> = None;
    if cfg.memory.enabled {
        match cfg.memory.backend {
            agent_config::MemoryBackend::Local => {
                let store = Arc::new(
                    agent_memory::LocalMemoryStore::new(&cwd)
                        .with_mental_models_config(agent_memory::MentalModelsConfig::default()),
                );
                local_memory = Some(Arc::clone(&store));
                memory = Some(store);
            }
            agent_config::MemoryBackend::Structured => {
                let store = Arc::new(
                    agent_memory::StructuredMemoryStore::new(&cwd)
                        .with_mental_models_config(agent_memory::MentalModelsConfig::default())
                        .with_embedder(agent_memory::default_embedder()),
                );
                structured_memory = Some(Arc::clone(&store));
                memory = Some(store);
            }
        }
    }
    let mut summarizer = agent_context::compaction::LlmSummaryProvider::new(
        Arc::clone(&provider),
        model.clone(),
        provider_ctx.clone(),
        cfg.compaction.remote_endpoint.clone(),
        cfg.user_agent.clone(),
    );
    if let Some(m) = &memory {
        // P2-5：压缩前召回（对齐 mnemopi `preCompactionContext`）——挂记忆到摘要提供器。
        summarizer = summarizer.with_memory(Arc::clone(m));
    }
    pctx.set_summarizer(Box::new(summarizer)).await;
    // Shake 归档落盘到 <cwd>/.gyre/artifacts（与 CLI 单次任务一致）。
    pctx.set_shake_sink(Arc::new(agent_context::compaction::DirSink::new(
        cwd.join(".gyre").join("artifacts"),
    )))
    .await;
    let context: Arc<dyn agent_core::ContextManager> = Arc::new(pctx);
    let workspace = Arc::new(agent_core::Workspace::new(cwd.clone()));
    // H41/H43：RPC 会话同样有进程内消息总线（hub 工具 + 子代理寻址）。
    let hub = agent_core::hub::Hub::new().shared();
    // H41：名册容量（0 = 不限；+1 计父 agent 自身）→ `task` 满员时在派生前预拒。
    hub.set_capacity(cfg.subagent.max_registry.saturating_add(1));
    // 进程托管 op（start/ps/logs/stop/restart/describe）与 CLI/server 对齐：随连接存续。
    let hub_processes = Arc::new(agent_supervisor::ProcessManager::new());
    // H33：PTY 执行器接入（`run_command(pty: true)`）。
    agent_tools::set_pty_executor(agent_pty::executor());
    // H7：todo / checkpoint 共享状态（工具注册与 `set_todos` / `get_todos` 同一实例）。
    let todo_state = agent_tools::TodoState::load(cwd.join(".gyre").join("todo.json")).shared();
    let checkpoint_state = agent_tools::CheckpointState::new().shared();
    // MCP 注册表：启动预算外的 server 转后台连接（有适用缓存快照者先以 Deferred 工具注册）；
    // 快照落盘与适用性判定（版本 + 配置指纹 + TTL）由注册表自理。
    let mcp_cache_dir = agent_core::platform::config_dir().unwrap_or_else(|| PathBuf::from("."));
    let mcp: Arc<agent_mcp::McpRegistry> = Arc::new(
        agent_mcp::McpRegistry::load(
            &cfg.mcp,
            &agent_mcp::McpLoadOptions {
                cache_dir: Some(mcp_cache_dir),
                // H16：把会话工作目录登记为 MCP root —— server 的 roots/list 才能拿到
                // 工作区范围（此前 server 请求一律被丢弃）。
                roots: vec![agent_mcp::McpRoot::file(&cwd, "workspace")],
                ..agent_mcp::McpLoadOptions::default()
            },
        )
        .await,
    );
    // MCP 工具动态源：装配层只持这一份句柄，父/子 Agent 注册表都挂它。
    let mcp_source: Arc<dyn agent_tools::ToolSource> =
        Arc::new(agent_mcp::McpToolSource::new(Arc::clone(&mcp)));
    // H42：可选组开关推导与 CLI/REPL、server 共用同一实现（agent-sdk）。
    let optional = agent_sdk::optional_tool_switches(&cfg);
    // 会话级文本快照存储：read_file（经 ToolContext::snapshots 记录）与 apply_hashline
    // 共享同一 Arc（与 main() 单次任务路径一致；跨 Agent 重建保持，会话级单例）。
    let snapshot_store = agent_sdk::new_snapshot_store();
    // H3 异步后台作业管理器（与 main() 会话路径同语义）。
    let job_manager = if cfg.agent.async_enabled {
        Some(agent_core::jobs::AsyncJobManager::with_max_running(
            cfg.agent.async_max_jobs,
        ))
    } else {
        None
    };

    // H3 投递 sink：完结作业结果以 async-result 通知形式追加进会话上下文
    //（对齐 omp registerDeliverySink 语义：owner 路由 + 恰好一次 + 失败退避重试）。
    let _job_sink_guard = job_manager.as_ref().map(|jm| {
        let ctx = Arc::clone(&context);
        let sink: agent_core::jobs::DeliverySink = Arc::new(move |job_id, text, job| {
            let ctx = Arc::clone(&ctx);
            Box::pin(async move {
                let label = job.as_ref().map(|j| j.label.clone()).unwrap_or_default();
                let status = job
                    .as_ref()
                    .map(|j| j.status.as_str())
                    .unwrap_or("completed");
                let body = format!(
                    "<system-notice>\n后台作业 {job_id}（{label}，{status}）已完结。请基于以下结果继续工作。\n\n{text}\n</system-notice>"
                );
                ctx.append(agent_core::AgentMessage::user_text(body)).await;
                Ok(())
            })
        });
        {
            let resumed = jm.register_delivery_sink("main", sink);
            // H27：Agent/会话重建后，把先前因 `watch ids` 被抑制的投递重新入队——
            // `enqueue_delivery` 在抑制状态下不入队，不恢复就会永久静默丢失结果。
            let suppressed = jm.suppressed_job_ids(Some("main"));
            if !suppressed.is_empty() {
                let refs: Vec<&str> = suppressed.iter().map(String::as_str).collect();
                jm.resume_deliveries(&refs);
                tracing::info!(jobs = suppressed.len(), "已恢复被抑制的作业投递");
            }
            resumed
        }
    });
    let (mut sub_reg, _) = assemble_builtin_tools(
        &optional,
        cfg.github.enabled,
        cfg.github.allow_write,
        cfg.agent.commands.interceptor.enabled,
        compiled_minimizer(&cfg),
        Some(Arc::clone(&snapshot_store)),
        job_manager.clone(),
    );
    // MCP 工具走动态源：server 端清单增删在子 Agent 下一轮即可见。
    sub_reg = sub_reg.with_source(Arc::clone(&mcp_source));
    let sub_tools: Arc<dyn agent_tools::ToolRegistry> = Arc::new(sub_reg);
    let sub_context_factory: agent::ContextFactory = Arc::new(|| {
        Arc::new(agent_context::InMemoryContext::new(vec![])) as Arc<dyn agent_core::ContextManager>
    });
    // H18：命名子代理定义 + 模型覆盖表（与 REPL/server 同一发现逻辑）。
    let sub_agents = agent::discover_agents(&cwd);
    let sub_model_overrides: std::collections::HashMap<String, agent_core::Model> = chain
        .iter()
        .flat_map(|p| {
            let m = p.to_model(cfg.agent.enable_thinking);
            let mut v = vec![(p.id.clone(), m.clone())];
            if let Some(a) = &p.alias {
                v.push((a.clone(), m));
            }
            v
        })
        .collect();

    // Skill 目录 + 上下文基座（H19/H42）：与 REPL / server 共用 `agent_sdk::compose_context_files`，
    // 此前此处漏接行为四段（§ Tool Policy / # Delegation / § Workflow / § Delivery）、
    // security_scan 指引与 MCP server 指引 → RPC 会话拿不到行为契约。
    let skill_catalog = Arc::new(load_skill_catalog(&cfg, &cwd).await);
    let mcp_instructions = mcp.server_instructions();
    let mut base_context_files = agent_sdk::compose_context_files(agent_sdk::ContextAssembly {
        cwd: &cwd,
        mcp_instructions: &mcp_instructions,
    })
    .files;
    // H18：可用命名子代理清单注入父级提示词。
    if let Some(section) = agent::render_agent_catalog(&sub_agents) {
        base_context_files.push(section);
    }

    // 反向通道（--rpc-forward-ask）：agent→host 审批/追问以 request 帧外发、宿主回答
    // 经 response 帧回路由。keepalive 克隆防止发送端全部先行 drop，主循环 recv() 永久
    // 返回 None 空转。
    let (reverse, mut reverse_rx) = ReverseChannel::new();
    let _reverse_keepalive = Arc::clone(&reverse);

    // 审批：默认 stdin 是协议通道，无法交互审批——一律拒绝（写工具会失败并反映在
    // tool_result 中）；需要全自动写权限时以 `--approval-mode yolo` 启动。
    // `--rpc-forward-ask` 启用时改走反向通道，把审批 / 追问交宿主决议（10 分钟超时按拒绝）。
    let prompt_resolver: agent_config::PromptResolver = if forward_ask {
        forward_ask_resolver(Arc::clone(&reverse))
    } else {
        Arc::new(|ask: agent_core::AskMessage| {
            Box::pin(async move {
                eprintln!("[rpc] 审批被自动拒绝（stdin 为协议通道）：{}", ask.prompt);
                Ok(AskResponse::No)
            })
        })
    };

    // 可变运行时状态（prompt 的 model 字段可覆盖）。
    let max_mistakes = cfg.agent.max_mistakes;
    let context_guard = cfg.agent.context_window_guard;
    let enable_thinking = cfg.agent.enable_thinking;
    let reasoning_budget = cfg.agent.reasoning_budget;
    let auto_thinking = cfg.agent.auto_thinking;
    let auto_thinking_model = cfg.agent.auto_thinking_model.clone();
    let auto_consolidate = cfg.memory.auto_consolidate;
    let subagent_enabled = cfg.subagent.enabled;
    let subagent_max_concurrent = cfg.subagent.max_concurrent;
    let subagent_inherit = cfg.subagent.inherit_parent;
    let subagent_max_output_override = cfg.subagent.max_output_tokens;
    let profile_temperature = profile.temperature;
    let github_enabled = cfg.github.enabled;
    let github_allow_write = cfg.github.allow_write;
    let supervisor = Arc::new(agent_supervisor::Supervisor::new());

    // P0-3 + H29：goals 状态共享句柄（跨 Agent 重建保持；与 main() 单次任务路径一致）。
    // H29 起恒在场（预算 0 = 不限），`goal` 工具与目标续跑无需 `[goals]` 配置。
    let goal_state: Option<Arc<std::sync::Mutex<agent::GoalState>>> =
        Some(Arc::new(std::sync::Mutex::new({
            let mut st = agent::GoalState::new(agent::GoalBudget {
                token_budget: cfg.goals.token_budget,
                time_budget: std::time::Duration::from_secs(cfg.goals.time_budget_secs),
                hard_stop: cfg.goals.hard_stop,
            });
            // H29：恢复持久化目标（`active` → `paused`，不自动续跑）。
            if let Some(snap) = agent::load_goal(&cwd) {
                st.restore(snap);
                tracing::info!(goal = ?st.goal_summary(), "已恢复持久化目标");
            }
            st
        })));

    // 会话控制共享状态（get_state / set_model / set_thinking / 轮次 / 流式标记）。
    let control = Arc::new(SessionControl {
        mode,
        cwd: cwd.clone(),
        todo: Arc::clone(&todo_state),
        checkpoints: Arc::clone(&checkpoint_state),
        session_id: session_id.clone(),
        session_file: session_path.clone(),
        alias: std::sync::Mutex::new(None),
        turns: AtomicU64::new(0),
        streaming: AtomicBool::new(false),
        compacting: Arc::new(AtomicBool::new(false)),
        thinking: Arc::new(RpcThinkingOverrides::new()),
        model: std::sync::Mutex::new(ModelRuntime {
            api_key: api_key.clone().unwrap_or_default(),
            base_url: profile.effective_base_url(),
            max_output: profile.max_output_tokens.unwrap_or(4096),
            model: model.clone(),
            provider_ctx: provider_ctx.clone(),
        }),
    });

    // Agent 构造闭包：与 main() 的单次任务路径一致（含 TTSR / advisor / 长期记忆 / 子 Agent）。
    #[allow(clippy::too_many_arguments)]
    let build_agent = |mode: agent_core::Mode,
                       model: agent_core::Model,
                       provider_ctx: agent_core::ProviderCallContext,
                       max_output: usize,
                       context: Arc<dyn agent_core::ContextManager>,
                       github_enabled: bool,
                       github_allow_write: bool,
                       optional: &std::collections::HashMap<String, bool>|
     -> agent::Agent {
        let mut agent_cfg = cfg.agent.clone();
        agent_cfg.mode = mode;
        let rules = agent_config::RulesEngine::new(Arc::new(agent_cfg))
            .with_workspace_root(Some(workspace.root().to_path_buf()));
        let approval: Arc<dyn agent_core::ApprovalPolicy> = Arc::new(
            agent_config::RulesApprovalPolicy::new(rules, Arc::clone(&prompt_resolver)),
        );
        let sub_temperature = if subagent_inherit {
            profile_temperature
        } else {
            None
        };
        let sub_thinking = if subagent_inherit && enable_thinking {
            Some(agent_core::ThinkingConfig::new(
                reasoning_budget.unwrap_or(16_000),
            ))
        } else {
            None
        };
        let sub_max_output = subagent_max_output_override.unwrap_or(max_output);
        let (mut tool_registry, lsp_pool) = assemble_builtin_tools(
            optional,
            github_enabled,
            github_allow_write,
            cfg.agent.commands.interceptor.enabled,
            compiled_minimizer(&cfg),
            Some(Arc::clone(&snapshot_store)),
            job_manager.clone(),
        );
        // H7 附带修复：RPC 前端此前**完全没有** todo/ask/checkpoint/rewind/security_scan
        // 五个工具（REPL 与 server 都有）→ 工具面三前端不一致、`set_todos` 也无宿主状态。
        // 状态句柄复用闭包外共享实例（跨 Agent 重建存活）。
        tool_registry = tool_registry
            .with(Box::new(agent_tools::TodoTool::new(Arc::clone(
                &todo_state,
            ))))
            .with(Box::new(agent_tools::AskUserTool::new()))
            .with(Box::new(agent_tools::CheckpointTool::new(Arc::clone(
                &checkpoint_state,
            ))))
            .with(Box::new(agent_tools::RewindTool::new(Arc::clone(
                &checkpoint_state,
            ))))
            .with(Box::new(agent_tools::SecurityScanTool::new(
                agent_tools::SecurityScanState::new().shared(),
            )));
        // H29：goal 工具（与 `/goal`、引擎续跑共享同一 GoalState）。
        if let Some(gs) = &goal_state {
            tool_registry = tool_registry.with(Box::new(agent::GoalTool::new(Arc::clone(gs))));
        }
        // H41/H43 续：hub 消息总线此前只在 CLI REPL 与 server 装配，RPC 前端缺 HubTool，
        // 导致 TaskTool 注册的子 Agent（task-N）在 RPC 下无法被父 Agent 寻址（单向）。
        // 以 "main" 入册，与前端身份一致；supervisor 复用本闭包外的实例。
        let hub_supervision: Arc<dyn agent_core::hub::HubSupervision> = supervisor.clone();
        tool_registry = tool_registry.with(Box::new(
            agent_tools::HubTool::register(Arc::clone(&hub), "main")
                .with_supervision(hub_supervision)
                .with_processes(Arc::clone(&hub_processes))
                .with_jobs_option(job_manager.clone()),
        ));
        // MCP 工具走动态源（每次 specs()/get() 实时求值）。
        tool_registry = tool_registry.with_source(Arc::clone(&mcp_source));
        // 记忆工具面（P0-1）：recall / retain（仅父 Agent；子 Agent 工具集不含）。
        if let Some(m) = &memory {
            tool_registry = tool_registry
                .with(Box::new(agent_tools::MemoryRecallTool::new(Arc::clone(m))))
                .with(Box::new(agent_tools::MemoryRetainTool::new(Arc::clone(m))));
        }
        if subagent_enabled {
            let task_tool = agent::TaskTool::new(
                Arc::clone(&provider),
                Arc::clone(&sub_tools),
                Arc::clone(&prompts),
                Arc::clone(&workspace),
                model.clone(),
                provider_ctx.clone(),
                mode,
                max_mistakes,
                context_guard,
                sub_max_output,
                Arc::clone(&sub_context_factory),
                sub_temperature,
                sub_thinking,
                subagent_max_concurrent,
            )
            .with_supervisor(Arc::clone(&supervisor))
            .with_approval(Arc::clone(&approval))
            .with_agents(sub_agents.clone())
            .with_model_overrides(sub_model_overrides.clone())
            .with_hub(Arc::clone(&hub));
            tool_registry = tool_registry.with(Box::new(task_tool));
        }
        let tools: Arc<dyn agent_tools::ToolRegistry> = Arc::new(tool_registry);

        // TTSR 流规则：发现 `<cwd>/.gyre/rules/*.md` 并装配协调器（与 main() 一致）。
        let ttsr = if cfg.ttsr.enabled.unwrap_or(true) {
            // H44：内置规则集（27 条语言约定）默认加载，同名项目规则覆盖之。
            let builtin_enabled = cfg.ttsr.builtin_rules.unwrap_or(true);
            let rules =
                agent_ttsr::load_rules(&workspace.root().join(".gyre/rules"), builtin_enabled);
            if rules.is_empty() {
                None
            } else {
                crate::report_ttsr_rules(&rules);
                Some(Arc::new(agent_ttsr::TtsrCoordinator::new(
                    agent_ttsr::TtsrConfig {
                        disabled_rules: cfg.ttsr.disabled_rules.clone(),
                        ..Default::default()
                    },
                    rules,
                )))
            }
        } else {
            None
        };

        // P1-L：advisor 独立评审（env `GYRE_ADVISOR=1` 启用；与 main() 一致）。
        let advisor = if std::env::var("GYRE_ADVISOR").is_ok_and(|v| v == "1") {
            let watchdog = agent_advisor::render_watchdog(&workspace.root());
            let adv = agent_advisor::Advisor::new(Arc::clone(&provider), model.clone());
            let adv = if watchdog.is_empty() {
                adv
            } else {
                adv.with_watchdog(watchdog)
            };
            eprintln!("已启用 advisor（每 {} 轮独立评审）", 4);
            Some(Arc::new(adv))
        } else {
            None
        };

        let builder = agent::Agent::builder(model.clone())
            .steering()
            .provider(Arc::clone(&provider))
            .tools(tools)
            .context(Arc::clone(&context))
            .prompts(Arc::clone(&prompts))
            .approval(Arc::clone(&approval))
            .workspace(Arc::clone(&workspace))
            .provider_ctx(provider_ctx.clone())
            .fallbacks(fallback_models.clone())
            .key_rings(key_rings.clone())
            .mode(mode)
            .ttsr(ttsr)
            .stream_guards(cfg.agent.stream_guards.clone())
            .advisor(advisor)
            .max_output_tokens(model.max_output_tokens)
            .max_mistakes(max_mistakes)
            .context_guard(context_guard)
            .compaction_policy(agent::AgentBuilder::compaction_policy_from_config(
                context_guard,
                cfg.agent.compaction_threshold_tokens,
                cfg.agent.compaction_reserve_tokens,
            ))
            .catalog(Arc::clone(&skill_catalog))
            .context_files(optional_context_files(
                &base_context_files,
                optional,
                github_enabled,
            ))
            .resources(Arc::clone(&mcp) as Arc<dyn agent_core::ResourceResolver>)
            // set_thinking：运行期思考预算覆盖，每轮 LLM 请求前解析（无需重建 Agent）。
            // 显式落型：Arc<RpcThinkingOverrides> → Arc<dyn RuntimeOverrides> 的 unsize 强转
            // 在此固定，避免内联表达式里的推断歧义。
            .runtime_overrides({
                let overrides: Arc<dyn agent::RuntimeOverrides> = control.thinking.clone();
                overrides
            });
        // P2：压缩后端——snapcompact 要求视觉模型（与 main() 一致；每次重建按当前 model 判断）。
        let compaction_backend = {
            let backend = agent_config::parse_compaction_backend(&cfg.compaction.backend);
            if backend == agent_core::CompactionBackend::Snapcompact {
                let vision = cfg.compaction.vision_models.is_empty()
                    || cfg
                        .compaction
                        .vision_models
                        .iter()
                        .any(|p| agent_config::wildcard_match(p, &model.id));
                if vision {
                    backend
                } else {
                    agent_core::CompactionBackend::Summarize
                }
            } else {
                backend
            }
        };
        let builder = builder
            .compaction_backend(compaction_backend)
            .compaction_max_frames(cfg.compaction.max_frames);
        // P0-3：goals 预算注入（共享状态，与 main() 一致）。
        let builder = if let Some(gs) = &goal_state {
            builder.goals_state(Arc::clone(gs))
        } else {
            builder
        };
        // H40：追加 system prompt 定制段（与 main() 一致）。
        let builder = if let Some(extra) = &cfg.agent.append_system_prompt {
            builder.append_system_prompt(extra.clone())
        } else {
            builder
        };
        // H28 尾项：注入异步唤醒探测（与 main() 一致）。
        let builder = if let Some(jm) = &job_manager {
            builder.async_wake(Arc::clone(jm) as Arc<dyn agent_core::AsyncWakeProbe>)
        } else {
            builder
        };
        // H28：todo 循环接线（eager prelude + 完成提醒；与 `todo` 工具共享同一清单）。
        let builder = builder
            .todo_loop_source(Arc::clone(&todo_state) as Arc<dyn agent_core::TodoLoopSource>)
            .todo_loop_config(agent::TodoLoopConfig::from_config(
                cfg.todo.eager.as_deref(),
                cfg.todo.reminders,
                cfg.todo.reminders_max,
                cfg.todo.mid_run_nudge,
            ));
        // 编辑后 LSP writethrough：lsp 启用（lsp_pool 为 Some）且 edit 开启时注入（与 main() 一致）。
        let builder = if lsp_pool.is_some()
            && (cfg.agent.tools.edit.format_on_write || cfg.agent.tools.edit.diagnostics_on_write)
        {
            match &lsp_pool {
                Some(pool) => {
                    builder.write_effect(std::sync::Arc::new(agent_tools::LspWriteEffect::new(
                        workspace.root().to_path_buf(),
                        std::sync::Arc::clone(pool),
                        cfg.agent.tools.edit.format_on_write,
                        cfg.agent.tools.edit.diagnostics_on_write,
                        cfg.agent.tools.edit.diagnostics_deduplicate,
                    ))
                        as std::sync::Arc<dyn agent_core::WriteEffect>)
                }
                None => builder, // 理论不可达：外层已检查 lsp_pool.is_some()
            }
        } else {
            builder
        };
        let builder = if let Some(m) = &memory {
            builder.memory(Arc::clone(m))
        } else {
            builder
        };
        // 记忆 hooks（P0-2/P0-4）：auto-retain（每 N 个停止轮）+ 会话末维护。
        // local：ConsolidateHook（LLM 合并，原语义保留）；structured：StructuredSleepHook。
        let mut hooks: Vec<Arc<dyn agent_core::Hook>> = Vec::new();
        if let Some(m) = &local_memory {
            hooks.push(Arc::new(ConsolidateHook {
                store: Arc::clone(m),
                provider: Arc::clone(&provider),
                model: model.clone(),
                provider_ctx: provider_ctx.clone(),
                auto_consolidate,
                background: cfg.memory.background_consolidate,
            }));
        }
        if let Some(m) = &structured_memory {
            hooks.push(Arc::new(StructuredSleepHook {
                store: Arc::clone(m),
                auto_consolidate: cfg.memory.auto_consolidate,
                provider: Some(Arc::clone(&provider)),
                model: Some(model.clone()),
                provider_ctx: Some(provider_ctx.clone()),
                background: cfg.memory.background_consolidate,
            }));
        }
        if let Some(m) = &memory {
            if cfg.memory.auto_retain_every_n_turns > 0 {
                hooks.push(Arc::new(AutoRetainHook {
                    store: Arc::clone(m),
                    every_n_turns: cfg.memory.auto_retain_every_n_turns,
                    final_rounds: std::sync::atomic::AtomicUsize::new(0),
                }));
            }
        }
        let builder = if hooks.is_empty() {
            builder
        } else {
            builder.hooks(hooks)
        };
        if enable_thinking {
            let static_cfg = agent_core::ThinkingConfig::new(reasoning_budget.unwrap_or(16_000));
            if auto_thinking {
                if let Some(tiny_id) = auto_thinking_model.clone() {
                    let mut tiny = model.clone();
                    tiny.id = tiny_id;
                    let classifier = Arc::new(agent_llm::LlmThinkingClassifier::new(
                        Arc::clone(&provider),
                        tiny,
                        provider_ctx.clone(),
                    ));
                    builder
                        .thinking_policy(agent_core::ThinkingPolicy::auto(classifier, static_cfg))
                        .build()
                } else {
                    tracing::warn!(
                        "auto_thinking 已启用但 auto_thinking_model 未配置，回退静态思考预算"
                    );
                    builder.thinking(static_cfg).build()
                }
            } else {
                builder.thinking(static_cfg).build()
            }
        } else {
            builder.build()
        }
    };

    // 以当前运行时模型状态构建 Agent（首次 prompt / prompt.model 切换 / set_model 共用）。
    let build_with_runtime = |control: &SessionControl| -> agent::Agent {
        let rt = control.model.lock().expect("模型运行时锁");
        build_agent(
            control.mode,
            rt.model.clone(),
            rt.provider_ctx.clone(),
            rt.max_output,
            Arc::clone(&context),
            github_enabled,
            github_allow_write,
            &optional,
        )
    };

    // ── SIGINT：持久注册句柄，避免信号在 select 轮询间隙丢失 ──
    let (sigint_tx, mut sigint_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = sigint_tx.send(()).await;
    });

    // ── 主循环：逐行读 stdin 请求，逐行写 stdout 响应 ──
    // compact 完成通知通道：后台压缩任务的唯一合法回帧路径（stdout 单写者约束）。
    let (compact_tx, mut compact_rx) = tokio::sync::mpsc::channel::<CompactNotice>(4);
    let stdin_reader: StdinReader = Arc::new(tokio::sync::Mutex::new(tokio::io::BufReader::new(
        tokio::io::stdin(),
    )));
    let mut out = tokio::io::BufWriter::new(tokio::io::stdout());
    let mut buf = String::new();
    let mut agent: Option<agent::Agent> = None;
    let mut decoder = RpcFrameDecoder::default();

    // ── 协议握手：accept 后、处理任何请求前先发 ready 帧（对齐 oh-my-pi rpc-mode.ts
    //    启动序列；客户端以此识别服务端能力，缺失即视为启动失败）──
    RPC_V2_NEGOTIATED.store(false, Ordering::Relaxed);
    write_frame(&mut out, &RpcReadyFrame::new()).await?;

    loop {
        tokio::select! {
            biased;
            n = read_line(&stdin_reader, &mut buf) => {
                let n = n?;
                if n == 0 {
                    break; // stdin EOF → 正常退出
                }
                if buf.trim().is_empty() {
                    continue;
                }
            }
            notice = compact_rx.recv() => {
                // 压缩完成通知：补发 event 行（turn 运行期间入队，空闲后送达）。
                if let Some(notice) = notice {
                    write_frame(&mut out, &RpcEnvelope::event(notice.id, notice.event)).await?;
                }
                continue;
            }
            line = reverse_rx.recv() => {
                // 反向通道排空：agent→host request 行原样落 stdout（单写者约束内）。
                if let Some(line) = line {
                    write_frame(&mut out, &line).await?;
                }
                continue;
            }
        }
        let line = buf.trim().to_string();
        let frame = match decode_inbound_line(&mut decoder, &line) {
            Ok(frame) => frame,
            Err(msg) => {
                write_frame(&mut out, &RpcEnvelope::error(0, msg)).await?;
                continue;
            }
        };
        // 分片未凑齐：无请求可处理，继续读下一行。
        let Some(frame) = frame else { continue };
        let req = match serde_json::from_value::<RpcRequest>(frame) {
            Ok(r) => r,
            Err(_) => {
                write_frame(&mut out, &RpcEnvelope::error(0, "invalid json request")).await?;
                continue;
            }
        };
        match req.ty.as_str() {
            // H6：协议协商（omp `rpc-client.ts:449` 握手第二步）。成功即启用 v2 分帧。
            "negotiate_protocol" => {
                let id = RpcEnvelope::req_id(&req);
                let requested = req
                    .protocol_version
                    .as_ref()
                    .and_then(serde_json::Value::as_u64);
                let frame = match requested {
                    Some(2) => {
                        RPC_V2_NEGOTIATED.store(true, Ordering::Relaxed);
                        OmpResponseFrame::success(
                            id,
                            "negotiate_protocol",
                            Some(serde_json::json!({ "protocolVersion": 2 })),
                        )
                    }
                    Some(other) => OmpResponseFrame::failure(
                        id,
                        "negotiate_protocol",
                        format!("Unsupported RPC protocol version: {other}"),
                    ),
                    None => OmpResponseFrame::failure(
                        id,
                        "negotiate_protocol",
                        "missing or non-numeric protocolVersion",
                    ),
                };
                write_frame(&mut out, &frame).await?;
            }
            "response" => {
                // 宿主对反向请求的回答 → 待答表；无对应请求（迟到 / 重复）→ 忽略。
                let request_id = RpcEnvelope::req_id(&req);
                let value = req
                    .result
                    .unwrap_or_else(|| serde_json::json!({ "answer": "no" }));
                if !reverse.resolve(&request_id, value) {
                    tracing::debug!(id = %request_id, "宿主回答无对应反向请求，忽略");
                }
            }
            "ping" => {
                write_frame(&mut out, &RpcEnvelope::pong(RpcEnvelope::req_id(&req))).await?;
            }
            "cancel" => {
                // 空闲时无 turn 可取消 → 错误响应（运行中由 turn 循环处理）。
                write_frame(
                    &mut out,
                    &RpcEnvelope::error(RpcEnvelope::req_id(&req), "no running turn"),
                )
                .await?;
            }
            // H7：omp `steer` / `follow_up` —— 空闲时入队（omp 亦为「排队，下次 turn 生效」语义）。
            "steer" | "follow_up" => {
                let command = if req.ty == "steer" {
                    "steer"
                } else {
                    "follow_up"
                };
                let frame = match agent.as_ref() {
                    Some(a) => match steer_running_turn(a, req.text.as_deref(), command) {
                        Ok(()) => {
                            OmpResponseFrame::success(RpcEnvelope::req_id(&req), command, None)
                        }
                        Err(msg) => {
                            OmpResponseFrame::failure(RpcEnvelope::req_id(&req), command, msg)
                        }
                    },
                    None => OmpResponseFrame::failure(
                        RpcEnvelope::req_id(&req),
                        command,
                        "no session agent yet (send a prompt first)",
                    ),
                };
                write_frame(&mut out, &frame).await?;
            }
            "prompt" => {
                let Some(text) = req.text.clone().filter(|t| !t.is_empty()) else {
                    write_frame(
                        &mut out,
                        &RpcEnvelope::error(
                            RpcEnvelope::req_id(&req),
                            "prompt requires non-empty text",
                        ),
                    )
                    .await?;
                    continue;
                };
                // 可选模型切换（失败 → error 响应，不执行本轮；成功 → 重建 Agent）。
                if let Some(alias) = req.model.clone().filter(|m| !m.is_empty()) {
                    let switched = {
                        let mut rt = control.model.lock().expect("模型运行时锁");
                        // MutexGuard 字段借用同前：先落一次 &mut *rt 再按不相交字段传参。
                        let m = &mut *rt;
                        apply_model_switch(
                            &alias,
                            &cfg,
                            &oauth_client,
                            &rpc_config_dir,
                            &mut m.api_key,
                            &mut m.base_url,
                            &mut m.max_output,
                            &mut m.model,
                            &mut m.provider_ctx,
                        )
                    };
                    if !switched {
                        write_frame(
                            &mut out,
                            &RpcEnvelope::error(
                                RpcEnvelope::req_id(&req),
                                format!("model switch failed: {alias}"),
                            ),
                        )
                        .await?;
                        continue;
                    }
                    *control.alias.lock().expect("模型别名锁") = Some(alias);
                    agent = Some(build_with_runtime(&control));
                } else if agent.is_none() {
                    // 惰性构建：首次 prompt 前，模型/模式取配置默认。
                    agent = Some(build_with_runtime(&control));
                }
                let Some(a) = agent.as_ref() else {
                    anyhow::bail!("构建 agent 失败（不可达）");
                };
                if run_turn_rpc(
                    a,
                    RpcEnvelope::req_id(&req),
                    &text,
                    &stdin_reader,
                    &mut out,
                    &mut decoder,
                    &mut sigint_rx,
                    &control,
                    &context,
                    &cfg,
                    &oauth_client,
                    &rpc_config_dir,
                    &reverse,
                    &mut reverse_rx,
                )
                .await?
                {
                    break; // stdin EOF 或 SIGINT：本轮已收尾，退出进程
                }
            }
            ty if is_session_command(ty) => {
                let outcome = dispatch_session_command(
                    &req,
                    &control,
                    context.as_ref(),
                    &cfg,
                    &oauth_client,
                    &rpc_config_dir,
                )
                .await;
                match outcome {
                    ControlOutcome::Reply(env) => {
                        write_frame(&mut out, &env).await?;
                    }
                    ControlOutcome::ModelSwitched { model_id } => {
                        // 运行时模型状态已更新：重建 Agent 后回 ok 帧。
                        agent = Some(build_with_runtime(&control));
                        write_frame(
                            &mut out,
                            &RpcEnvelope::ok_model(RpcEnvelope::req_id(&req), &model_id),
                        )
                        .await?;
                    }
                    ControlOutcome::CompactionStarted => {
                        // 异步完成型：立即 ok，压缩序列后台执行，完成后经通道补发 event 行。
                        if control.compacting.swap(true, Ordering::Relaxed) {
                            write_frame(
                                &mut out,
                                &RpcEnvelope::error(
                                    RpcEnvelope::req_id(&req),
                                    "compaction already in progress",
                                ),
                            )
                            .await?;
                        } else {
                            write_frame(&mut out, &RpcEnvelope::ok(RpcEnvelope::req_id(&req)))
                                .await?;
                            tokio::spawn(run_compaction(
                                Arc::clone(&context),
                                Arc::clone(&control),
                                RpcEnvelope::req_id(&req),
                                compact_tx.clone(),
                            ));
                        }
                    }
                }
            }
            other => match unsupported_omp_command(RpcEnvelope::req_id(&req), other) {
                Some(frame) => write_frame(&mut out, &frame).await?,
                None => {
                    write_frame(
                        &mut out,
                        &RpcEnvelope::error(RpcEnvelope::req_id(&req), "unknown rpc message type"),
                    )
                    .await?;
                }
            },
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// 单测：协议编解码纯函数
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ContextManager as _;
    use agent_core::{AgentRunSummary, StatusKind, ToolResult};

    fn usage() -> Usage {
        Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            cost_usd: 0.5,
        }
    }

    /// 测试辅助：经完整入站解码路径（重组器直通）解析单行请求。
    fn parse_request(line: &str) -> std::result::Result<RpcRequest, String> {
        let mut decoder = RpcFrameDecoder::default();
        match decode_inbound_line(&mut decoder, line)? {
            Some(frame) => {
                serde_json::from_value(frame).map_err(|_| "invalid json request".to_string())
            }
            None => Err("incomplete chunk sequence".to_string()),
        }
    }

    /// 构造把 `frame` 均分为 256 KiB/片的 `rpc_chunk` 物理行序列（`frame` 字节数须整除）。
    fn chunk_lines(frame: &str, chunk_id: &str) -> Vec<String> {
        let bytes = frame.as_bytes();
        assert_eq!(bytes.len() % RPC_CHUNK_PAYLOAD_BYTES, 0);
        let count = bytes.len() / RPC_CHUNK_PAYLOAD_BYTES;
        (0..count)
            .map(|index| {
                serde_json::to_string(&RpcChunkFrame {
                    ty: "rpc_chunk",
                    chunk_id,
                    index,
                    count,
                    byte_length: bytes.len(),
                    data: &BASE64_STANDARD.encode(
                        &bytes[index * RPC_CHUNK_PAYLOAD_BYTES
                            ..(index + 1) * RPC_CHUNK_PAYLOAD_BYTES],
                    ),
                })
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn parses_prompt_request_with_optional_model() {
        let r = parse_request(r#"{"type":"prompt","id":7,"text":"hello","model":"ds"}"#).unwrap();
        assert_eq!(r.ty, "prompt");
        assert_eq!(r.id, Some(serde_json::json!(7)));
        assert_eq!(r.text.as_deref(), Some("hello"));
        assert_eq!(r.model.as_deref(), Some("ds"));
    }

    #[test]
    fn parses_request_without_optional_fields() {
        let r = parse_request(r#"{"type":"cancel","id":2}"#).unwrap();
        assert_eq!(r.ty, "cancel");
        assert_eq!(r.id, Some(serde_json::json!(2)));
        assert_eq!(r.text, None);
        assert_eq!(r.model, None);
    }

    #[test]
    fn parses_unknown_type_keeping_id() {
        let r = parse_request(r#"{"type":"explode","id":9}"#).unwrap();
        assert_eq!(r.ty, "explode");
        assert_eq!(r.id, Some(serde_json::json!(9)));
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_request("not json").is_err());
        assert!(parse_request(r#"{"type":"ping"}"#.trim_end_matches('}')).is_err());
    }

    #[test]
    fn encode_json_is_single_line_and_small_frames_pass_through() {
        let env = RpcEnvelope::event(
            1,
            serde_json::json!({"kind": "text_delta", "text": "a\nb\t\"c\""}),
        );
        let json = encode_json(&env).unwrap();
        // 序列化结果本身无换行（文本内换行已被转义）。
        assert!(!json.contains('\n'));
        // 小帧直通：恰好一条物理行、单个行尾换行，可回读且结构与事件一致。
        let lines: Vec<String> = encode_frame_lines(&json).collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].matches('\n').count(), 1);
        assert!(lines[0].ends_with('\n'));
        let back: serde_json::Value = serde_json::from_str(lines[0].trim_end()).unwrap();
        assert_eq!(back["type"], "event");
        assert_eq!(back["id"], 1);
        assert_eq!(back["event"]["kind"], "text_delta");
        assert_eq!(back["event"]["text"], "a\nb\t\"c\"");
    }

    // ── H8：事件映射宽度（id / result / 生命周期 / omp 形状） ───────────────────

    #[test]
    fn tool_events_carry_id_and_structured_result() {
        // 旧式形状：补 id 与 result（此前只有 name/ok）。
        let start = map_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call_1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "a.rs"}),
        })
        .unwrap();
        assert_eq!(start["kind"], "tool_call");
        assert_eq!(start["id"], "call_1");

        let end = map_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call_1".into(),
            name: "read_file".into(),
            result: agent_core::ToolResult::Text("content".into()),
            is_error: false,
        })
        .unwrap();
        assert_eq!(end["kind"], "tool_result");
        assert_eq!(end["id"], "call_1");
        assert_eq!(end["ok"], true);
        assert_eq!(end["result"]["content"], "content");

        // 生命周期边界此前完全未暴露。
        assert_eq!(
            map_event(&AgentEvent::TurnStart).unwrap()["kind"],
            "turn_start"
        );
        assert_eq!(
            map_event(&AgentEvent::MessageStart).unwrap()["kind"],
            "message_start"
        );
    }

    #[test]
    fn omp_event_shape_uses_camel_case_and_top_level_type() {
        let start = map_event_omp(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call_9".into(),
            name: "run_command".into(),
            args: serde_json::json!({"command": "ls"}),
        })
        .unwrap();
        assert_eq!(start["type"], "tool_execution_start");
        assert_eq!(start["toolCallId"], "call_9");
        assert_eq!(start["toolName"], "run_command");
        assert_eq!(start["args"]["command"], "ls");

        let end = map_event_omp(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call_9".into(),
            name: "run_command".into(),
            result: agent_core::ToolResult::Text("ok".into()),
            is_error: true,
        })
        .unwrap();
        assert_eq!(end["type"], "tool_execution_end");
        assert_eq!(end["isError"], true);
        assert_eq!(end["result"]["content"], "ok");

        // message_update 携带 assistantMessageEvent（不带 partial message：见文档偏差）。
        let delta = map_event_omp(&AgentEvent::TextDelta("hi".into())).unwrap();
        assert_eq!(delta["type"], "message_update");
        assert_eq!(delta["assistantMessageEvent"]["type"], "text_delta");
        assert_eq!(delta["assistantMessageEvent"]["delta"], "hi");

        // 会话事件扁平外发（自身即带 type 判别字段）。
        let session = map_event_omp(&AgentEvent::Session(
            agent_core::SessionEvent::AutoCompactionStart {
                reason: agent_core::CompactionReason::Threshold,
                action: agent_core::CompactionAction::Shake,
            },
        ))
        .unwrap();
        assert_eq!(session["type"], "auto_compaction_start");
    }

    /// H8：v2 对端拿裸帧（顶层 type），v1 对端拿旧式 event 包装。
    ///
    /// 测试串行化锁（进程级 `RPC_V2_NEGOTIATED` 不能并行改）用 `blocking_lock` 语义：
    /// 以 `std::sync::Mutex` 手写获取/释放，避免在 async 测试里跨 await 持有 guard。
    #[tokio::test]
    async fn event_line_shape_follows_negotiated_mode() {
        let guard = CHUNK_GATE_TEST_LOCK.lock().await;
        let ev = AgentEvent::TextDelta("hi".into());

        RPC_V2_NEGOTIATED.store(false, Ordering::Relaxed);
        let mut out = tokio::io::BufWriter::new(Vec::<u8>::new());
        write_agent_event(&mut out, &serde_json::json!(7), &ev)
            .await
            .unwrap();
        let text = String::from_utf8(out.into_inner()).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(v["type"], "event");
        assert_eq!(v["id"], 7);
        assert_eq!(v["event"]["kind"], "text_delta");

        RPC_V2_NEGOTIATED.store(true, Ordering::Relaxed);
        let mut out = tokio::io::BufWriter::new(Vec::<u8>::new());
        write_agent_event(&mut out, &serde_json::json!(7), &ev)
            .await
            .unwrap();
        let text = String::from_utf8(out.into_inner()).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(v["type"], "message_update");
        assert!(v.get("event").is_none());
        RPC_V2_NEGOTIATED.store(false, Ordering::Relaxed);
        drop(guard);
    }
    #[test]
    fn ready_frame_matches_omp_wire_shape() {
        // 字段名/取值对齐 omp RpcReadyFrame（rpc-types.ts）。
        let v = serde_json::to_value(RpcReadyFrame::new()).unwrap();
        assert_eq!(v["type"], "ready");
        assert_eq!(v["protocolVersion"], 1);
        assert_eq!(v["supportedProtocolVersions"], serde_json::json!([1, 2]));
        assert_eq!(v["maxFrameBytes"], MAX_RPC_FRAME_BYTES);
        assert_eq!(v["maxReassembledFrameBytes"], MAX_RPC_REASSEMBLED_BYTES);
    }

    // ── H6 / H9：协议协商、id 类型与信封兼容 ────────────────────────────────────

    #[test]
    fn request_accepts_string_and_numeric_ids() {
        // omp 官方客户端发字符串 id；Gyre 旧客户端发数字 id —— 两者都必须能解析。
        let with_str: RpcRequest =
            serde_json::from_value(serde_json::json!({"type":"ping","id":"abc-1"})).unwrap();
        assert_eq!(with_str.id, Some(serde_json::json!("abc-1")));
        assert_eq!(RpcEnvelope::req_id(&with_str), serde_json::json!("abc-1"));

        let with_num: RpcRequest =
            serde_json::from_value(serde_json::json!({"type":"ping","id":7})).unwrap();
        assert_eq!(RpcEnvelope::req_id(&with_num), serde_json::json!(7));

        // id 缺失（omp 的 id 可选）→ 按 null 回带，不再整帧反序列化失败。
        let no_id: RpcRequest = serde_json::from_value(serde_json::json!({"type":"ping"})).unwrap();
        assert_eq!(RpcEnvelope::req_id(&no_id), serde_json::Value::Null);

        // prompt 文本接受 omp 的 `message` 字段名。
        let omp_prompt: RpcRequest =
            serde_json::from_value(serde_json::json!({"type":"prompt","message":"hi"})).unwrap();
        assert_eq!(omp_prompt.text.as_deref(), Some("hi"));

        // negotiate_protocol 接受 omp 的 camelCase `protocolVersion`。
        let neg: RpcRequest = serde_json::from_value(
            serde_json::json!({"type":"negotiate_protocol","protocolVersion":2,"id":"n1"}),
        )
        .unwrap();
        assert_eq!(
            neg.protocol_version
                .as_ref()
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );
    }

    #[test]
    fn string_id_is_echoed_verbatim_in_envelope() {
        let env = RpcEnvelope::pong(serde_json::json!("req-42"));
        let v = serde_json::to_value(env).unwrap();
        assert_eq!(v["id"], "req-42");
        assert_eq!(v["type"], "pong");
    }

    #[test]
    fn omp_negotiate_response_envelope_shape() {
        // 成功：`{id, type:"response", command, success:true, data:{protocolVersion:2}}`
        let ok = OmpResponseFrame::success(
            serde_json::json!("n1"),
            "negotiate_protocol",
            Some(serde_json::json!({ "protocolVersion": 2 })),
        );
        let v = serde_json::to_value(ok).unwrap();
        assert_eq!(v["type"], "response");
        assert_eq!(v["command"], "negotiate_protocol");
        assert_eq!(v["success"], true);
        assert_eq!(v["data"]["protocolVersion"], 2);
        assert_eq!(v["id"], "n1");
        assert!(v.get("error").is_none());

        // 失败：`{id, type:"response", command, success:false, error}`
        let bad = OmpResponseFrame::failure(
            serde_json::json!("n2"),
            "negotiate_protocol",
            "Unsupported RPC protocol version: 9",
        );
        let v = serde_json::to_value(bad).unwrap();
        assert_eq!(v["success"], false);
        assert_eq!(v["command"], "negotiate_protocol");
        assert!(v["error"].as_str().unwrap().contains("9"));
        assert!(v.get("data").is_none());
        // 无 id 时不序列化 id 字段（对齐 omp `{id?: string}`）。
        let no_id = serde_json::to_value(OmpResponseFrame::success(
            serde_json::Value::Null,
            "negotiate_protocol",
            None,
        ))
        .unwrap();
        assert!(no_id.get("id").is_none());
    }

    /// H6：未协商 v2 时绝不分片（v1 客户端会把 `rpc_chunk` 当未知类型丢弃）。
    #[test]
    fn chunking_is_gated_on_negotiated_v2() {
        let _guard = CHUNK_GATE_TEST_LOCK.blocking_lock();
        let big = format!(
            "{{\"type\":\"event\",\"event\":\"{}\"}}",
            "x".repeat(MAX_RPC_FRAME_BYTES)
        );
        RPC_V2_NEGOTIATED.store(false, Ordering::Relaxed);
        let lines: Vec<String> = encode_frame_lines(&big).collect();
        assert_eq!(lines.len(), 1, "v1 不得分片");
        assert!(lines[0].contains("rpc_frame_error"), "{}", lines[0]);

        RPC_V2_NEGOTIATED.store(true, Ordering::Relaxed);
        let lines: Vec<String> = encode_frame_lines(&big).collect();
        assert!(lines.len() > 1, "v2 应分片");
        assert!(lines.iter().all(|l| l.contains("rpc_chunk")));
        // 复位，避免影响同进程其它用例。
        RPC_V2_NEGOTIATED.store(false, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn ready_frame_is_first_line_before_any_response() {
        // 内存写路径模拟帧流：ready 先于任何响应行（对齐 run_rpc 的启动序列）。
        let mut out = tokio::io::BufWriter::new(Vec::<u8>::new());
        write_frame(&mut out, &RpcReadyFrame::new()).await.unwrap();
        write_frame(&mut out, &RpcEnvelope::pong(3)).await.unwrap();
        let text = String::from_utf8(out.into_inner()).unwrap();
        let mut lines = text.lines();
        let first: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(first["type"], "ready");
        assert_eq!(first["protocolVersion"], PROTOCOL_VERSION);
        let second: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(second["type"], "pong");
        assert!(lines.next().is_none());
    }

    #[test]
    fn oversized_frame_splits_into_chunks_and_reassembles() {
        // ~2.5 MiB 逻辑帧：**协商 v2 后**超过单行上限 → 必须分片；逐行喂入重组器还原原帧。
        // （H6：v1 不再分片，见 `chunking_is_gated_on_negotiated_v2`。）
        let _guard = CHUNK_GATE_TEST_LOCK.blocking_lock();
        RPC_V2_NEGOTIATED.store(true, Ordering::Relaxed);
        let payload = "x".repeat(2 * 1024 * 1024 + 512 * 1024);
        let env = RpcEnvelope::event(
            7,
            serde_json::json!({"kind": "text_delta", "text": payload}),
        );
        let json = encode_json(&env).unwrap();
        assert!(json.len() + 1 > MAX_RPC_FRAME_BYTES);

        let lines: Vec<String> = encode_frame_lines(&json).collect();
        assert!(lines.len() >= 2);
        RPC_V2_NEGOTIATED.store(false, Ordering::Relaxed);
        let mut decoder = RpcFrameDecoder::default();
        let mut delivered = None;
        let mut chunk_id: Option<String> = None;
        for line in &lines {
            // 每条物理行都不得超过单行传输上限。
            assert!(line.len() <= MAX_RPC_FRAME_BYTES);
            let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
            if value["type"] == "rpc_chunk" {
                let cid = value["chunkId"].as_str().unwrap().to_string();
                match &chunk_id {
                    Some(prev) => assert_eq!(prev, &cid),
                    None => chunk_id = Some(cid),
                }
                assert_eq!(value["byteLength"], json.len());
                delivered = decoder.push(value).unwrap();
            } else {
                delivered = Some(value);
            }
        }
        assert!(chunk_id.unwrap().starts_with("rpc-"));
        let frame = delivered.expect("最后一个分片应交付完整帧");
        assert_eq!(frame["type"], "event");
        assert_eq!(frame["id"], 7);
        assert_eq!(
            frame["event"]["text"].as_str().unwrap().len(),
            2 * 1024 * 1024 + 512 * 1024
        );
    }

    #[test]
    fn out_of_order_chunks_reassemble() {
        // 1.5 MiB 逻辑帧切成 6 片 256 KiB，倒序到达：乱序容忍，最后一片到齐即交付。
        let text_len = 1536 * 1024 - 24; // 恰使整帧 = 6 × 256 KiB
        let frame = format!("{{\"kind\":\"big\",\"text\":\"{}\"}}", "y".repeat(text_len));
        assert!(frame.len() >= MAX_RPC_FRAME_BYTES);
        let chunks = chunk_lines(&frame, "rpc-test");
        assert_eq!(chunks.len(), 6);
        let mut decoder = RpcFrameDecoder::default();
        for (i, line) in chunks.iter().rev().enumerate() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let delivered = decoder.push(value).unwrap();
            if i + 1 < chunks.len() {
                assert!(delivered.is_none());
            } else {
                let back = delivered.expect("末片到达即交付");
                assert_eq!(back["kind"], "big");
                assert_eq!(back["text"].as_str().unwrap().len(), text_len);
            }
        }
    }

    #[test]
    fn duplicate_chunk_frame_is_ignored() {
        // 同一 (chunkId, index) 重复帧幂等忽略：对端重发不破坏交付。
        let frame = format!(
            "{{\"kind\":\"big\",\"text\":\"{}\"}}",
            "y".repeat(1536 * 1024 - 24)
        );
        let chunks = chunk_lines(&frame, "rpc-test");
        let mut decoder = RpcFrameDecoder::default();
        let first: serde_json::Value = serde_json::from_str(&chunks[0]).unwrap();
        assert!(decoder.push(first.clone()).unwrap().is_none());
        assert!(decoder.push(first).unwrap().is_none()); // 重复帧，不计入
        for line in &chunks[1..] {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let delivered = decoder.push(value).unwrap();
            if line == chunks.last().unwrap() {
                assert_eq!(delivered.unwrap()["kind"], "big");
            } else {
                assert!(delivered.is_none());
            }
        }
    }

    #[test]
    fn plain_frames_pass_through_decoder() {
        // 旧客户端从不发分片：连续普通帧一律直通（向后兼容）。
        let mut decoder = RpcFrameDecoder::default();
        let v: serde_json::Value = serde_json::from_str(r#"{"type":"ping","id":3}"#).unwrap();
        assert_eq!(decoder.push(v.clone()).unwrap().unwrap(), v);
        assert_eq!(
            decoder
                .push(serde_json::json!({"type":"ping","id":4}))
                .unwrap()
                .unwrap(),
            serde_json::json!({"type":"ping","id":4})
        );
    }

    #[test]
    fn plain_frame_inside_chunk_sequence_interrupts() {
        // 对齐 omp：序列未交付前到达普通帧 → 协议错误；残片丢弃后恢复直通。
        let frame = format!(
            "{{\"kind\":\"big\",\"text\":\"{}\"}}",
            "y".repeat(1536 * 1024 - 24)
        );
        let chunks = chunk_lines(&frame, "rpc-test");
        let mut decoder = RpcFrameDecoder::default();
        let first: serde_json::Value = serde_json::from_str(&chunks[0]).unwrap();
        assert!(decoder.push(first).unwrap().is_none());
        let err = decoder
            .push(serde_json::json!({"type":"ping","id":1}))
            .unwrap_err();
        assert_eq!(err, "rpc chunk sequence interrupted");
        // 残片已丢弃：普通帧恢复直通。
        assert!(
            decoder
                .push(serde_json::json!({"type":"ping","id":2}))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn chunk_metadata_mismatch_errors() {
        // 同 chunkId 但 byteLength 前后不一致 → sequence mismatch（对齐 omp）。
        let frame = format!(
            "{{\"kind\":\"big\",\"text\":\"{}\"}}",
            "y".repeat(1536 * 1024 - 24)
        );
        let chunks = chunk_lines(&frame, "rpc-test");
        let mut decoder = RpcFrameDecoder::default();
        assert!(
            decoder
                .push(serde_json::from_str::<serde_json::Value>(&chunks[0]).unwrap())
                .unwrap()
                .is_none()
        );
        let bad = serde_json::json!({
            "type": "rpc_chunk",
            "chunkId": "rpc-test",
            "index": 1,
            "count": 6,
            "byteLength": frame.len() as u64 + 1,
            "data": BASE64_STANDARD.encode(b"xyz"),
        });
        assert_eq!(
            decoder.push(bad).unwrap_err(),
            "rpc chunk sequence mismatch"
        );
    }

    #[test]
    fn invalid_chunk_metadata_is_rejected() {
        let mut decoder = RpcFrameDecoder::default();
        let data = BASE64_STANDARD.encode(b"x");
        // count < 2：单帧不该分片。
        let one = serde_json::json!({
            "type": "rpc_chunk", "chunkId": "rpc-1", "index": 0, "count": 1,
            "byteLength": MAX_RPC_FRAME_BYTES as u64, "data": data,
        });
        assert_eq!(decoder.push(one).unwrap_err(), "invalid rpc chunk metadata");
        // 逻辑帧未超单行上限也不该分片。
        let small = serde_json::json!({
            "type": "rpc_chunk", "chunkId": "rpc-1", "index": 0, "count": 2,
            "byteLength": 16_u64, "data": data,
        });
        assert_eq!(
            decoder.push(small).unwrap_err(),
            "invalid rpc chunk metadata"
        );
        // 非法 base64 拒绝。
        let bad_b64 = serde_json::json!({
            "type": "rpc_chunk", "chunkId": "rpc-1", "index": 0, "count": 2,
            "byteLength": MAX_RPC_FRAME_BYTES as u64, "data": "!!!!",
        });
        assert_eq!(decoder.push(bad_b64).unwrap_err(), "invalid rpc chunk data");
    }

    #[test]
    fn frame_beyond_reassembly_limit_falls_back_to_error_frame() {
        // 超重组上限（64 MiB）：放弃传输，回退 rpc_frame_error（对齐 omp overflowFrame）。
        let _guard = CHUNK_GATE_TEST_LOCK.blocking_lock();
        RPC_V2_NEGOTIATED.store(true, Ordering::Relaxed);
        let json = format!("{{\"text\":\"{}\"}}", "z".repeat(MAX_RPC_REASSEMBLED_BYTES));
        let lines: Vec<String> = encode_frame_lines(&json).collect();
        RPC_V2_NEGOTIATED.store(false, Ordering::Relaxed);
        assert_eq!(lines.len(), 1);
        let v: serde_json::Value = serde_json::from_str(lines[0].trim_end()).unwrap();
        assert_eq!(v["type"], "rpc_frame_error");
        assert_eq!(v["error"], "RPC frame exceeded the transport limit");
    }

    #[test]
    fn envelope_shapes_match_contract() {
        // done ok：带 usage 与 turns，无 error/message。
        let v: serde_json::Value =
            serde_json::to_value(RpcEnvelope::done_ok(1, &usage(), 3)).unwrap();
        assert_eq!(v["type"], "done");
        assert_eq!(v["id"], 1);
        assert_eq!(v["ok"], true);
        assert_eq!(v["turns"], 3);
        assert_eq!(v["usage"]["input"], 1);
        assert_eq!(v["usage"]["output"], 2);
        assert_eq!(v["usage"]["cache_read"], 3);
        assert_eq!(v["usage"]["cache_write"], 4);
        assert_eq!(v["usage"]["cost"], 0.5);
        assert!(v.get("error").is_none());
        assert!(v.get("message").is_none());
        assert!(v.get("event").is_none());

        // done err：ok=false + error，无 usage/turns。
        let v: serde_json::Value =
            serde_json::to_value(RpcEnvelope::done_err(1, "cancelled")).unwrap();
        assert_eq!(v["type"], "done");
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "cancelled");
        assert!(v.get("usage").is_none());
        assert!(v.get("turns").is_none());

        // error 响应：message 字段。
        let v: serde_json::Value =
            serde_json::to_value(RpcEnvelope::error(2, "unknown rpc message type")).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["id"], 2);
        assert_eq!(v["message"], "unknown rpc message type");

        // pong。
        let v: serde_json::Value = serde_json::to_value(RpcEnvelope::pong(3)).unwrap();
        assert_eq!(v["type"], "pong");
        assert_eq!(v["id"], 3);
    }

    #[test]
    fn maps_text_and_thinking_deltas() {
        let v = map_event(&AgentEvent::TextDelta("hi".into())).unwrap();
        assert_eq!(v["kind"], "text_delta");
        assert_eq!(v["text"], "hi");
        let v = map_event(&AgentEvent::ThinkingDelta("t".into())).unwrap();
        assert_eq!(v["kind"], "thinking_delta");
        assert_eq!(v["text"], "t");
    }

    #[test]
    fn maps_status_and_tool_lifecycle() {
        let v = map_event(&AgentEvent::Say(agent_core::StatusMessage {
            text: "working".into(),
            kind: StatusKind::Info,
        }))
        .unwrap();
        assert_eq!(v["kind"], "status");
        assert_eq!(v["text"], "working");

        let v = map_event(&AgentEvent::ToolExecutionStart {
            tool_call_id: "1".into(),
            name: "run_command".into(),
            args: serde_json::json!({"cmd": "ls"}),
        })
        .unwrap();
        assert_eq!(v["kind"], "tool_call");
        assert_eq!(v["name"], "run_command");
        assert_eq!(v["args"]["cmd"], "ls");

        let ok = map_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "1".into(),
            name: "run_command".into(),
            result: ToolResult::text("done"),
            is_error: false,
        })
        .unwrap();
        assert_eq!(ok["kind"], "tool_result");
        assert_eq!(ok["name"], "run_command");
        assert_eq!(ok["ok"], true);

        let err = map_event(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "1".into(),
            name: "run_command".into(),
            result: ToolResult::text("boom"),
            is_error: true,
        })
        .unwrap();
        assert_eq!(err["ok"], false);
    }

    /// 结构化会话事件：`kind=session` 包裹 `AgentSessionEvent` 同形对象（判别字段 `type`）。
    #[test]
    fn maps_session_events() {
        let v = map_event(&AgentEvent::Session(
            agent_core::SessionEvent::AutoRetryStart {
                attempt: 2,
                max_attempts: 3,
                delay_ms: 1500,
                error_message: "429".into(),
                model: "m1".into(),
            },
        ))
        .unwrap();
        assert_eq!(v["kind"], "session");
        assert_eq!(v["event"]["type"], "auto_retry_start");
        assert_eq!(v["event"]["attempt"], 2);

        let v = map_event(&AgentEvent::Session(
            agent_core::SessionEvent::RetryFallbackApplied {
                from: "a".into(),
                to: "b".into(),
            },
        ))
        .unwrap();
        assert_eq!(v["event"]["type"], "retry_fallback_applied");
        assert_eq!(v["event"]["from"], "a");
        assert_eq!(v["event"]["to"], "b");
    }

    #[test]
    fn maps_usage_with_all_five_fields() {
        let v = map_event(&AgentEvent::Usage(usage())).unwrap();
        assert_eq!(v["kind"], "usage");
        assert_eq!(v["input"], 1);
        assert_eq!(v["output"], 2);
        assert_eq!(v["cache_read"], 3);
        assert_eq!(v["cache_write"], 4);
        assert_eq!(v["cost"], 0.5);
    }

    #[test]
    fn hides_lifecycle_events() {
        // H8 起 `TurnStart` / `TurnEnd` / `MessageStart` / `MessageEnd` **不再隐藏**：
        // 消费者需要精确划分边界（见 `tool_events_carry_id_and_structured_result`）。
        // 仍然隐藏的：`Done` / `Error`（turn 成败由最终 done 行承载）、
        // `StateChanged`（UI 状态机内部信号）、`Assistant`（增量已由 TextDelta 覆盖）。
        assert!(map_event(&AgentEvent::Done(AgentRunSummary::default())).is_none());
        assert!(map_event(&AgentEvent::Error("x".into())).is_none());
        assert!(map_event(&AgentEvent::StateChanged(agent_core::AgentState::Idle)).is_none());
    }

    #[test]
    fn sanitizes_non_finite_cost() {
        let mut u = usage();
        u.cost_usd = f64::NAN;
        let v = usage_json(&u);
        assert_eq!(v["cost"], 0.0);
    }
    // ── 会话控制命令（get_state / set_model / set_thinking / get_messages / compact /
    // get_usage）：内存上下文上的帧 → 响应断言 ──

    /// 测试配置：默认链 + 别名 `alt` 的第二 profile（set_model 成功路径用）。
    fn test_cfg() -> agent_config::Config {
        serde_json::from_str::<agent_config::Config>(
            r#"{
                "default_model": {"id": "m1", "alias": "m1", "api": "openai-completions", "base_url": "http://localhost"},
                "models": [{"id": "m2", "alias": "alt", "api": "openai-completions", "base_url": "http://localhost"}]
            }"#,
        )
        .expect("测试配置应可解析")
    }

    /// 测试模型（与 test_cfg 的默认链一致）。
    fn test_model() -> agent_core::Model {
        agent_core::Model {
            id: "m1".into(),
            provider: "openai-compatible".into(),
            api: agent_core::Api::OpenAiCompletions,
            max_input_tokens: 128_000,
            max_output_tokens: 4096,
            supports_tools: true,
            supports_streaming: true,
            supports_thinking: false,
            extra_body: None,
            tokenizer: None,
        }
    }

    /// 测试辅助：会话控制共享状态（轮次预置 3、模型 m1；不落盘）。
    fn test_control() -> SessionControl {
        SessionControl {
            mode: agent_core::Mode::Code,
            cwd: PathBuf::from("/tmp"),
            todo: agent_tools::TodoState::in_memory().shared(),
            checkpoints: agent_tools::CheckpointState::new().shared(),
            session_id: "sess-test".into(),
            session_file: PathBuf::from("/tmp/sess-test.jsonl"),
            alias: std::sync::Mutex::new(None),
            turns: AtomicU64::new(3),
            streaming: AtomicBool::new(false),
            compacting: Arc::new(AtomicBool::new(false)),
            thinking: Arc::new(RpcThinkingOverrides::new()),
            model: std::sync::Mutex::new(ModelRuntime {
                api_key: "k".into(),
                base_url: "http://localhost".into(),
                max_output: 4096,
                model: test_model(),
                provider_ctx: agent_core::ProviderCallContext {
                    api_key: Some("k".into()),
                    base_url: Some("http://localhost".into()),
                    max_in_flight: None,
                    headers: Vec::new(),
                    quirks: agent_core::ProviderQuirks::default(),
                    auth: agent_core::AuthMode::ApiKey,
                },
            }),
        }
    }

    /// 测试辅助：预置一条用户消息 + 一条带用量的助手长文本消息的内存上下文。
    async fn seeded_context() -> agent_context::InMemoryContext {
        let ctx = agent_context::InMemoryContext::new(vec![]);
        ctx.append(AgentMessage::user_text("你好，介绍自己")).await;
        ctx.append(AgentMessage::Assistant(agent_core::AssistantMessage {
            content: vec![agent_core::ContentBlock::Text {
                text: "x".repeat(MESSAGE_PREVIEW_CHARS * 3),
            }],
            usage: Usage {
                input_tokens: 11,
                output_tokens: 7,
                cost_usd: 0.25,
                ..Default::default()
            },
            model: "m1".into(),
            stop_reason: None,
            stop_details: None,
        }))
        .await;
        ctx
    }

    /// H7：`get_session_stats` 返回 omp `SessionStats` 形状的子集。
    #[tokio::test]
    async fn get_session_stats_reports_counts_and_usage() {
        let control = test_control();
        let ctx = seeded_context().await;
        let env = dispatch_line(&control, &ctx, r#"{"type":"get_session_stats","id":21}"#).await;
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["type"], "state");
        assert_eq!(v["id"], 21);
        let stats = &v["state"];
        assert_eq!(stats["sessionId"], "sess-test");
        assert_eq!(stats["turns"], 3);
        assert_eq!(stats["userMessages"], 1);
        assert_eq!(stats["assistantMessages"], 1);
        assert_eq!(stats["toolResults"], 0);
        assert!(stats["toolCalls"].is_number());
        assert_eq!(stats["totalMessages"], 2);
        assert!(stats["contextUsage"]["current"].is_number());
        assert_eq!(stats["isStreaming"], false);
    }

    /// H7：`get_available_commands` 返回内置斜杠命令清单（omp 同名命令）。
    #[tokio::test]
    async fn get_available_commands_lists_builtins() {
        let control = test_control();
        let ctx = agent_context::InMemoryContext::new(vec![]);
        let env = dispatch_line(
            &control,
            &ctx,
            r#"{"type":"get_available_commands","id":22}"#,
        )
        .await;
        let v = serde_json::to_value(&env).unwrap();
        let names: Vec<&str> = v["state"]["commands"]
            .as_array()
            .expect("commands 应为数组")
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        let sources: Vec<&str> = v["state"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["source"].as_str())
            .collect();
        assert!(names.contains(&"help"), "应含内置命令: {names:?}");
        assert!(
            names.iter().all(|n| !n.starts_with('/')),
            "omp 的 name 不带前导斜杠: {names:?}"
        );
        assert!(names.len() > 10, "内置命令面不应为空: {names:?}");
        assert!(sources.contains(&"builtin"), "应标注 source=builtin");
    }

    /// H7：新命令进入会话命令白名单（否则运行中被判 unknown）。
    #[test]
    fn session_command_whitelist_covers_new_commands() {
        for ty in [
            "get_session_stats",
            "get_available_commands",
            "steer",
            "follow_up",
            "abort",
            "get_state",
        ] {
            assert!(
                is_session_command(ty) || matches!(ty, "steer" | "follow_up" | "abort"),
                "{ty} 应被 RPC 分发识别"
            );
        }
        assert!(!is_session_command("frobnicate"));
    }

    /// H7：steer 文本校验（空消息 / 非法负载不得静默吞掉）。
    #[test]
    fn steer_requires_non_empty_message() {
        // 空文本在触达 Agent 之前就被拒绝（无需构造 Agent）。
        assert!(steer_running_turn_guard(None).is_err());
        assert!(steer_running_turn_guard(Some("   ")).is_err());
    }

    /// `steer_running_turn` 的空文本分支（不触达 Agent，避免构造完整 Agent）。
    fn steer_running_turn_guard(text: Option<&str>) -> Result<(), String> {
        let text = text.unwrap_or_default().trim();
        if text.is_empty() {
            return Err("steer requires a non-empty message".to_string());
        }
        Ok(())
    }
    /// H7：`set_todos` 双形状（omp phases / Gyre items）+ `get_todos` 回读。
    #[tokio::test]
    async fn set_todos_accepts_omp_phases_and_reads_back() {
        let control = test_control();
        let ctx = agent_context::InMemoryContext::new(vec![]);
        let env = dispatch_line(
            &control,
            &ctx,
            r#"{"type":"set_todos","id":31,"todos":{"phases":[
                {"name":"阶段一","tasks":[
                    {"content":"写测试","status":"in_progress"},
                    {"content":"等评审","status":"blocked","blocker":"等 CI"}
                ]}
            ]}}"#,
        )
        .await;
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["type"], "state");
        let items = v["state"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], "t1");
        assert_eq!(items[0]["content"], "写测试");
        assert_eq!(items[0]["phase"], "in_progress");
        assert_eq!(items[1]["phase"], "blocked");
        assert_eq!(items[1]["blocked_reason"], "等 CI");
        // 分组名无 Gyre 对应 → 显式 note（不静默丢弃）。
        assert!(v["state"]["note"].as_str().unwrap().contains("展平"));

        // get_todos 回读同一共享状态（`todo` 工具与 RPC 面同一实例）。
        let env = dispatch_line(&control, &ctx, r#"{"type":"get_todos","id":32}"#).await;
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["state"]["items"][0]["content"], "写测试");
        assert_eq!(v["state"]["nextId"], 3);

        // 非法状态 / 缺字段 → 明确错误（不写入）。
        let env = dispatch_line(
            &control,
            &ctx,
            r#"{"type":"set_todos","id":33,"todos":{"phases":[{"tasks":[{"content":"x","status":"bogus"}]}]}}"#,
        )
        .await;
        assert_eq!(env.ty, "error");
        let env = dispatch_line(&control, &ctx, r#"{"type":"set_todos","id":34}"#).await;
        assert_eq!(env.ty, "error");
    }

    /// H7：Gyre 原生 `items` 形状同样受理。
    #[tokio::test]
    async fn set_todos_accepts_gyre_items() {
        let control = test_control();
        let ctx = agent_context::InMemoryContext::new(vec![]);
        let env = dispatch_line(
            &control,
            &ctx,
            r#"{"type":"set_todos","id":35,"todos":{"items":[{"content":"a","phase":"pending"}]}}"#,
        )
        .await;
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["state"]["items"][0]["content"], "a");
        assert!(v["state"].get("note").is_none());
    }
    /// H7：`get_available_models` 来自**配置**（非网络），`get_last_assistant_text` 取末条助手文本。
    #[tokio::test]
    async fn get_available_models_and_last_assistant_text() {
        let control = test_control();
        let ctx = seeded_context().await;
        let env = dispatch_line(&control, &ctx, r#"{"type":"get_available_models","id":41}"#).await;
        let v = serde_json::to_value(&env).unwrap();
        let models = v["state"]["models"].as_array().unwrap();
        assert!(!models.is_empty(), "配置至少应有默认模型");
        assert_eq!(models[0]["isDefault"], true);
        assert!(models[0]["id"].is_string());

        let env = dispatch_line(
            &control,
            &ctx,
            r#"{"type":"get_last_assistant_text","id":42}"#,
        )
        .await;
        let v = serde_json::to_value(&env).unwrap();
        assert!(v["state"]["text"].as_str().unwrap().starts_with('x'));
    }

    /// H7：未实现的 omp 命令回 omp 信封 + 明确原因（而非 legacy unknown 错误）。
    #[test]
    fn recognized_unsupported_omp_commands_answer_with_omp_envelope() {
        let frame = unsupported_omp_command(serde_json::json!("c1"), "bash").unwrap();
        let v = serde_json::to_value(frame).unwrap();
        assert_eq!(v["type"], "response");
        assert_eq!(v["command"], "bash");
        assert_eq!(v["success"], false);
        assert!(v["error"].as_str().unwrap().contains("未实现"));
        assert_eq!(v["id"], "c1");
        // 真·未知命令不在此清单（仍走 legacy unknown）。
        assert!(unsupported_omp_command(serde_json::json!(1), "frobnicate").is_none());
        // 已实现的命令不得出现在「不支持」清单里（防清单漂移）。
        for implemented in [
            "prompt",
            "steer",
            "follow_up",
            "abort",
            "get_state",
            "get_session_stats",
            "get_available_commands",
            "get_available_models",
            "get_last_assistant_text",
            "set_todos",
            "get_todos",
            "set_model",
            "compact",
            "get_messages",
            "negotiate_protocol",
        ] {
            assert!(
                !OMP_RECOGNIZED_UNSUPPORTED.contains(&implemented),
                "{implemented} 已实现，不应列入不支持清单"
            );
        }
    }
    /// 测试辅助：解析一行请求并派发到会话控制层，要求得到立即响应帧。
    async fn dispatch_line(
        control: &SessionControl,
        context: &agent_context::InMemoryContext,
        line: &str,
    ) -> RpcEnvelope {
        let req = parse_request(line).expect("请求行应可解析");
        let cfg = test_cfg();
        match dispatch_session_command(
            &req,
            control,
            context,
            &cfg,
            &reqwest::Client::new(),
            std::path::Path::new("."),
        )
        .await
        {
            ControlOutcome::Reply(env) => env,
            other => panic!("预期 Reply 帧，实际 {other:?}"),
        }
    }

    #[test]
    fn parses_session_command_requests() {
        let r = parse_request(r#"{"type":"set_model","id":1,"alias":"ds"}"#).unwrap();
        assert_eq!(r.alias.as_deref(), Some("ds"));
        let r = parse_request(r#"{"type":"set_thinking","id":2,"budget":2048}"#).unwrap();
        assert_eq!(r.budget, Some(serde_json::json!(2048)));
        let r = parse_request(r#"{"type":"set_thinking","id":3,"budget":null}"#).unwrap();
        assert_eq!(r.budget, Some(serde_json::Value::Null));
        let r = parse_request(r#"{"type":"get_messages","id":4}"#).unwrap();
        assert_eq!(r.limit, None);
        let r = parse_request(r#"{"type":"get_messages","id":5,"limit":10}"#).unwrap();
        assert_eq!(r.limit, Some(serde_json::json!(10)));
        assert!(is_session_command("get_state") && is_session_command("get_usage"));
        assert!(!is_session_command("prompt") && !is_session_command("ping"));
    }

    #[test]
    fn session_command_frames_match_contract() {
        // ok / ok_budget（null = 关闭思考）/ state / messages / usage 的线协议形状。
        let v = serde_json::to_value(RpcEnvelope::ok(1)).unwrap();
        assert_eq!(v, serde_json::json!({"type":"ok","id":1,"ok":true}));
        let v = serde_json::to_value(RpcEnvelope::ok_budget(2, None)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type":"ok","id":2,"ok":true,"budget":null})
        );
        let v = serde_json::to_value(RpcEnvelope::ok_budget(3, Some(4096))).unwrap();
        assert_eq!(v["budget"], 4096);
        let v = serde_json::to_value(RpcEnvelope::state(4, serde_json::json!({"mode":"code"})))
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type":"state","id":4,"state":{"mode":"code"}})
        );
        let v = serde_json::to_value(RpcEnvelope::messages(
            5,
            serde_json::json!([{"role":"user","text":"hi"}]),
        ))
        .unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
        let v = serde_json::to_value(RpcEnvelope::usage(6, usage_json(&usage()))).unwrap();
        assert_eq!(v["usage"]["input"], 1);
    }

    #[test]
    fn parse_budget_and_limit_validate_params() {
        // budget：数字 / null / 缺失 / 类型非法。
        assert_eq!(
            parse_budget(&Some(serde_json::json!(2048))).unwrap(),
            Some(2048)
        );
        assert_eq!(parse_budget(&Some(serde_json::Value::Null)).unwrap(), None);
        assert_eq!(
            parse_budget(&None).unwrap_err(),
            "set_thinking requires budget (number or null)"
        );
        assert_eq!(
            parse_budget(&Some(serde_json::json!("big"))).unwrap_err(),
            "budget must be a non-negative integer or null"
        );
        // limit：缺省 → 默认值；超限 → 钳制；类型非法 → 错误文案。
        assert_eq!(parse_limit(&None).unwrap(), DEFAULT_MESSAGES_LIMIT);
        assert_eq!(parse_limit(&Some(serde_json::json!(2))).unwrap(), 2);
        assert_eq!(
            parse_limit(&Some(serde_json::json!(u64::MAX))).unwrap(),
            MAX_MESSAGES_LIMIT
        );
        assert_eq!(
            parse_limit(&Some(serde_json::json!("many"))).unwrap_err(),
            "limit must be a positive integer"
        );
    }

    #[tokio::test]
    async fn get_state_reports_model_mode_session_and_turns() {
        let control = test_control();
        let ctx = seeded_context().await;
        let env = dispatch_line(&control, &ctx, r#"{"type":"get_state","id":11}"#).await;
        assert_eq!(env.ty, "state");
        assert_eq!(env.id, 11);
        let v = serde_json::to_value(&env).unwrap();
        let s = &v["state"];
        assert_eq!(s["model"]["id"], "m1");
        assert_eq!(s["model"]["provider"], "openai-compatible");
        assert_eq!(s["model"]["maxInputTokens"], 128_000);
        assert_eq!(s["model"]["maxOutputTokens"], 4096);
        assert_eq!(s["modelAlias"], serde_json::Value::Null);
        assert_eq!(s["mode"], "code");
        assert_eq!(s["sessionId"], "sess-test");
        assert_eq!(s["sessionFile"], "/tmp/sess-test.jsonl");
        assert_eq!(s["turns"], 3);
        assert_eq!(s["messageCount"], 2);
        assert!(s["contextTokens"].is_u64());
        assert_eq!(s["thinkingBudget"], serde_json::Value::Null);
        assert_eq!(s["isStreaming"], false);
        assert_eq!(s["isCompacting"], false);
    }

    #[tokio::test]
    async fn get_messages_returns_recent_previews_with_limit() {
        let control = test_control();
        let ctx = seeded_context().await;
        // limit=1：只取最近 1 条（助手长文本 → 截断预览，含省略号）。
        let env = dispatch_line(
            &control,
            &ctx,
            r#"{"type":"get_messages","id":12,"limit":1}"#,
        )
        .await;
        assert_eq!(env.ty, "messages");
        let v = serde_json::to_value(&env).unwrap();
        let arr = v["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["role"], "assistant");
        let text = arr[0]["text"].as_str().unwrap();
        assert!(text.ends_with('…'));
        assert_eq!(text.chars().count(), MESSAGE_PREVIEW_CHARS + 1);
        // 无 limit：默认上限内全量返回（此处 2 条），顺序保持根→叶。
        let env = dispatch_line(&control, &ctx, r#"{"type":"get_messages","id":13}"#).await;
        let v = serde_json::to_value(&env).unwrap();
        let arr = v["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["text"], "你好，介绍自己");
        // 非法 limit → 错误帧回带 id。
        let env = dispatch_line(
            &control,
            &ctx,
            r#"{"type":"get_messages","id":14,"limit":"many"}"#,
        )
        .await;
        assert_eq!(env.ty, "error");
        assert_eq!(env.id, 14);
    }

    #[tokio::test]
    async fn set_thinking_updates_shared_budget_and_replies() {
        let control = test_control();
        let ctx = seeded_context().await;
        // 数字预算 → ok 帧 + 共享状态更新 + RuntimeOverrides 每轮解析生效。
        let env = dispatch_line(
            &control,
            &ctx,
            r#"{"type":"set_thinking","id":15,"budget":4096}"#,
        )
        .await;
        assert_eq!(env.ty, "ok");
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["budget"], 4096);
        assert_eq!(control.thinking.snapshot(), Some(4096));
        assert_eq!(
            agent::RuntimeOverrides::thinking(&*control.thinking, &test_model())
                .map(|c| c.budget_tokens),
            Some(4096)
        );
        assert_eq!(
            agent::RuntimeOverrides::disable_thinking(&*control.thinking),
            None
        );
        // budget: null → 关闭思考（disable_thinking 命中）。
        let env = dispatch_line(
            &control,
            &ctx,
            r#"{"type":"set_thinking","id":16,"budget":null}"#,
        )
        .await;
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["budget"], serde_json::Value::Null);
        assert_eq!(
            agent::RuntimeOverrides::disable_thinking(&*control.thinking),
            Some(true)
        );
        assert!(agent::RuntimeOverrides::thinking(&*control.thinking, &test_model()).is_none());
        // 缺参 → 错误帧回带 id。
        let env = dispatch_line(&control, &ctx, r#"{"type":"set_thinking","id":17}"#).await;
        assert_eq!(env.ty, "error");
        assert_eq!(env.id, 17);
        assert_eq!(
            env.message.as_deref(),
            Some("set_thinking requires budget (number or null)")
        );
    }

    #[tokio::test]
    async fn set_model_switches_runtime_records_alias_or_errors() {
        let control = test_control();
        let ctx = seeded_context().await;
        let cfg = test_cfg();
        // 成功：ModelSwitched + 运行时模型与别名已更新。
        let req = parse_request(r#"{"type":"set_model","id":21,"alias":"alt"}"#).unwrap();
        match dispatch_session_command(
            &req,
            &control,
            &ctx,
            &cfg,
            &reqwest::Client::new(),
            std::path::Path::new("."),
        )
        .await
        {
            ControlOutcome::ModelSwitched { model_id } => assert_eq!(model_id, "m2"),
            other => panic!("预期 ModelSwitched，实际 {other:?}"),
        }
        assert_eq!(control.alias.lock().unwrap().as_deref(), Some("alt"));
        assert_eq!(control.model.lock().unwrap().model.id, "m2");
        // 未知别名 → error 帧（与 prompt.model 同语义文案）。
        let req = parse_request(r#"{"type":"set_model","id":22,"alias":"nope"}"#).unwrap();
        match dispatch_session_command(
            &req,
            &control,
            &ctx,
            &cfg,
            &reqwest::Client::new(),
            std::path::Path::new("."),
        )
        .await
        {
            ControlOutcome::Reply(env) => {
                assert_eq!(env.ty, "error");
                assert_eq!(env.id, 22);
                assert_eq!(env.message.as_deref(), Some("model switch failed: nope"));
            }
            other => panic!("预期 Reply 帧，实际 {other:?}"),
        }
        // 缺 alias → error 帧回带 id。
        let req = parse_request(r#"{"type":"set_model","id":23}"#).unwrap();
        match dispatch_session_command(
            &req,
            &control,
            &ctx,
            &cfg,
            &reqwest::Client::new(),
            std::path::Path::new("."),
        )
        .await
        {
            ControlOutcome::Reply(env) => {
                assert_eq!(env.ty, "error");
                assert_eq!(env.id, 23);
            }
            other => panic!("预期 Reply 帧，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_usage_reports_accumulated_usage() {
        let control = test_control();
        let ctx = seeded_context().await;
        let env = dispatch_line(&control, &ctx, r#"{"type":"get_usage","id":31}"#).await;
        assert_eq!(env.ty, "usage");
        let v = serde_json::to_value(&env).unwrap();
        // 累计用量 = 活跃路径 assistant 消息 usage 之和（现有 accumulated 形状）。
        assert_eq!(v["usage"]["input"], 11);
        assert_eq!(v["usage"]["output"], 7);
        assert_eq!(v["usage"]["cost"], 0.25);
    }

    #[tokio::test]
    async fn compact_acknowledges_and_notifies_completion() {
        let ctx = agent_context::InMemoryContext::new(vec![]);
        let control = Arc::new(test_control());
        let cfg = test_cfg();
        let req = parse_request(r#"{"type":"compact","id":41}"#).unwrap();
        match dispatch_session_command(
            &req,
            &control,
            &ctx,
            &cfg,
            &reqwest::Client::new(),
            std::path::Path::new("."),
        )
        .await
        {
            ControlOutcome::CompactionStarted => {}
            other => panic!("预期 CompactionStarted，实际 {other:?}"),
        }
        // 后台压缩序列（shake → summarize → prune）完成后经通道送达通知。
        let ctx: Arc<dyn agent_core::ContextManager> = Arc::new(ctx);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<CompactNotice>(1);
        run_compaction(
            Arc::clone(&ctx),
            Arc::clone(&control),
            serde_json::json!(41),
            tx,
        )
        .await;
        let notice = rx.recv().await.expect("应收到完成通知");
        assert_eq!(notice.id, 41);
        assert_eq!(notice.event["kind"], "compact_done");
        assert!(notice.event["tokens"]["current"].is_u64());
        // 互斥标记已复位；通知以现有事件帧风格回写。
        assert!(!control.compacting.load(Ordering::Relaxed));
        let v = serde_json::to_value(RpcEnvelope::event(notice.id, notice.event)).unwrap();
        assert_eq!(v["type"], "event");
        assert_eq!(v["event"]["kind"], "compact_done");
    }
    #[test]
    fn parse_host_answer_maps_variants() {
        // AskResponse 未派生 PartialEq：以模式匹配断言变体。
        assert!(matches!(
            parse_host_answer(&serde_json::json!({ "answer": "yes" })),
            AskResponse::Yes
        ));
        assert!(matches!(
            parse_host_answer(&serde_json::json!({ "answer": "no" })),
            AskResponse::No
        ));
        assert!(matches!(
            parse_host_answer(&serde_json::json!({ "answer": "text", "text": "改成 ls" })),
            AskResponse::Text(t) if t == "改成 ls"
        ));
        // 畸形 / 缺失 / 未知 answer → 拒绝（宁拒勿挂）。
        assert!(matches!(
            parse_host_answer(&serde_json::json!({})),
            AskResponse::No
        ));
        assert!(matches!(
            parse_host_answer(&serde_json::json!({ "answer": "maybe" })),
            AskResponse::No
        ));
        assert!(matches!(
            parse_host_answer(&serde_json::json!({ "answer": "text" })),
            AskResponse::Text(t) if t.is_empty()
        ));
    }

    #[tokio::test]
    async fn reverse_channel_roundtrip_and_routing() {
        let (channel, mut rx) = ReverseChannel::new();
        let ask_msg = AskMessage {
            id: "ask-1".into(),
            kind: AskKind::Tool {
                tool: "run_command".into(),
            },
            prompt: "允许执行？".into(),
        };
        let spawned = Arc::clone(&channel);
        let h = tokio::spawn(async move { spawned.ask(&ask_msg).await });
        // 宿主侧：收到 request 行（形状 + 反向 id 命名空间独立）。
        let line = rx.recv().await.expect("应收到反向请求行");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "request");
        assert_eq!(v["method"], "ask");
        assert_eq!(v["params"]["kind"], "tool");
        assert_eq!(v["params"]["tool"], "run_command");
        assert_eq!(v["params"]["prompt"], "允许执行？");
        let rid = v["id"].as_u64().unwrap();
        // 未注册 id（迟到 / 重复）→ false；命中后 ask 侧收到 yes。
        assert!(!channel.resolve(
            &serde_json::json!(rid + 999),
            serde_json::json!({ "answer": "no" })
        ));
        assert!(channel.resolve(
            &serde_json::json!(rid),
            serde_json::json!({ "answer": "yes" })
        ));
        assert!(matches!(h.await.unwrap(), AskResponse::Yes));
        // 已消费的 id 再答 → false。
        assert!(!channel.resolve(&serde_json::json!(rid), serde_json::json!({"answer": "no"})));
    }
    #[tokio::test]
    async fn reverse_channel_denies_when_host_gone() {
        let (channel, rx) = ReverseChannel::new();
        drop(rx); // 宿主侧消失：出向发送失败 → 立即拒绝，不悬挂。
        let ask_msg = AskMessage {
            id: "ask-2".into(),
            kind: AskKind::Followup,
            prompt: "还在吗？".into(),
        };
        assert!(matches!(channel.ask(&ask_msg).await, AskResponse::No));
    }

    #[test]
    fn ask_kind_tags_are_stable() {
        // 线协议标签锁定：宿主按 kind 分派 UI，改名即破坏协议。
        let tool = AskKind::Tool { tool: "x".into() };
        let command = AskKind::Command {
            command: "ls".into(),
        };
        assert_eq!(ask_kind_tag(&tool), "tool");
        assert_eq!(ask_kind_tag(&command), "command");
        assert_eq!(ask_kind_tag(&AskKind::Followup), "followup");
        assert_eq!(
            ask_kind_tag(&AskKind::CompletionResult),
            "completion_result"
        );
    }
}
