//! DAP 客户端错误类型（中文 message，thiserror）。

/// DAP 客户端错误。
#[derive(Debug, thiserror::Error)]
pub enum DapError {
    /// 适配器进程启动失败（命令不存在 / 权限 / 资源）。
    #[error("适配器进程启动失败: {0}")]
    SpawnFailed(String),

    /// 帧编解码或协议握手违背 DAP 规范（含适配器返回的错误响应）。
    #[error("DAP 协议错误: {0}")]
    Protocol(String),

    /// 请求发出后未在时限内收到配对响应。
    #[error("DAP 请求超时（{0}）")]
    Timeout(String),

    /// 没有活动的调试会话（先 `debug launch` / `debug attach`）。
    #[error("没有活动的调试会话，请先执行 debug launch 或 debug attach")]
    NoSession,

    /// 未找到调试适配器（PATH 探测失败）。
    #[error("未找到调试适配器 {0}")]
    AdapterNotFound(String),

    /// 会话已关闭（适配器进程退出 / 显式 close / 写失败）。
    #[error("调试会话已关闭: {0}")]
    Closed(String),

    /// 底层 IO 错误。
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON 编解码错误。
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
