//! eval 子系统错误类型。

/// eval 子系统错误：内核/桥/协议层面的故障（用户代码错误经 [`crate::EvalOutput::error`]
/// 返回，不落入本枚举）。
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// 找不到 Python 解释器。
    #[error("未找到 Python 解释器：{0}（请安装 python3 或配置 [eval] python 路径）")]
    PythonMissing(String),
    /// 找不到 Node.js 运行时。
    #[error("未找到 Node.js 运行时：{0}（请安装 Node.js ≥18 或配置 [eval] node 路径）")]
    NodeMissing(String),
    /// 内核进程启动失败。
    #[error("内核启动失败：{0}")]
    KernelSpawnFailed(String),
    /// 不支持的语言。
    #[error("不支持的语言：{0}（当前仅支持 \"py\"/\"js\"）")]
    UnsupportedLanguage(String),
    /// 环回桥错误。
    #[error("环回桥错误：{0}")]
    BridgeError(String),
    /// 单次执行超时。
    #[error("内核执行超时（超过 {0:?} 未返回，内核已回收）")]
    Timeout(std::time::Duration),
    /// 内核进程意外退出。
    #[error("内核进程意外退出")]
    KernelDied,
    /// NDJSON 序列化/反序列化错误。
    #[error("序列化错误：{0}")]
    Serde(String),
    /// 标准库 IO 错误。
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 执行被取消。
    #[error("eval 执行被取消")]
    Canceled,
}
