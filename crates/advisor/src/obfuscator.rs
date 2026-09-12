//! Secret 脱敏（H45：实现收敛到 `agent_core::PatternRedactor`）。
//!
//! 本模块此前自带一份正则脱敏实现，与 `crates/core/src/secrets.rs` 的能力重复。
//! 现只保留**类型别名再导出**，让 advisor 的公共 API（`SecretObfuscator`）不变，
//! 实现单源于 core。
//!
//! 语义差异记录：core 的 [`SecretsObfuscator`](agent_core::SecretsObfuscator) 是**可逆**
//! HMAC 占位符（provider 消息往返还原用）；本别名指向**不可逆**的模式掩码
//! [`PatternRedactor`](agent_core::PatternRedactor)，两者用途不同、不合并。

pub use agent_core::PatternRedactor as SecretObfuscator;
