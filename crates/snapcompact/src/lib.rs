//! 图像化上下文压缩（snapcompact）：把被丢弃的历史序列化为紧凑文本，渲染成
//! 位图字体 PNG 帧，视觉模型直接读图回放——完全本地、确定性、无 LLM 调用。
//!
//! 对标 oh-my-pi `packages/snapcompact`（TS 渲染 + pi-natives Rust 栅格化）的
//! 最小 Rust 核心：v1 采用单一形状（8px 列宽 × 16px 行高、1568² 帧），白底黑字，
//! Liberation Mono 字形（内嵌 TTF，SIL Open Font License 兼容的自由许可）。
//! CJK 与不可渲染码点折叠为 `?`（中文会话内容可读性损失为 v1 已知取舍）。
//!
//! # 模块
//! - [`render`]：PNG 帧渲染（`ab_glyph` 栅格化 + `image` 编码）。
//! - [`serialize`]：文本归一化与分页（ANSI 剥离 / 空白折叠 / 容量分页）。
//! - [`shape`]：帧形状、帧数预算与 provider 图像预算（移植 snapcompact.ts 常量）。

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

pub mod render;
pub mod serialize;
pub mod shape;

pub use render::{render_frame, Frame};
pub use serialize::{normalize, paginate, SerializeOptions};
pub use shape::{
    estimate_frame_tokens, max_frames_for_data_budget, provider_image_budget, Shape,
    DEFAULT_MAX_FRAMES, DEFAULT_SHAPE, FRAME_DATA_BYTES_BUDGET, FRAME_TOKEN_ESTIMATE,
};
