//! 帧形状与预算：移植 oh-my-pi snapcompact.ts 的帧几何与 provider 图像预算常量。

/// 帧几何：8px 列宽 × 16px 行高（8x13 字形 + 3px 行距，OMP `8on16-bw` 变体语义），
/// 1568² 帧（eval 验证的阅读尺寸）。行数 = 1568 / 16 = 98，列数 = 1568 / 8 = 196。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// 单元格列宽（px）。
    pub cell_w: u32,
    /// 单元格行高（px）。
    pub cell_h: u32,
    /// 帧边长（px，方形帧）。
    pub frame_size: u32,
}

impl Shape {
    /// 默认形状（OMP 8on16-bw：1568² 帧、8×16 单元）。
    #[must_use]
    pub const fn default_shape() -> Self {
        Self {
            cell_w: 8,
            cell_h: 16,
            frame_size: 1568,
        }
    }

    /// 每帧可容纳的行数。
    #[must_use]
    pub const fn rows(self) -> usize {
        (self.frame_size / self.cell_h) as usize
    }

    /// 每行可容纳的列数（字符）。
    #[must_use]
    pub const fn cols(self) -> usize {
        (self.frame_size / self.cell_w) as usize
    }

    /// 每帧字符容量（行 × 列）。
    #[must_use]
    pub const fn capacity(self) -> usize {
        self.rows() * self.cols()
    }
}

/// 默认形状常量。
pub const DEFAULT_SHAPE: Shape = Shape::default_shape();

/// 默认压缩携带帧数上界：约 400k token 的高分辨率帧预算（OMP `MAX_FRAMES_DEFAULT`）。
pub const DEFAULT_MAX_FRAMES: usize = 80;

/// 单帧 token 估算的保守上界（OMP `FRAME_TOKEN_ESTIMATE`：高分辨率帧触 4,784 视觉
/// token 上限 × 1.05 边距）。用于上下文预算，防溢出护栏低估。
pub const FRAME_TOKEN_ESTIMATE: usize = 5024;

/// 单帧 base64 负载保守上界（OMP `FRAME_DATA_BYTES_ESTIMATE`，实测 ~159KB 留边距）。
const FRAME_DATA_BYTES_ESTIMATE: usize = 170_000;

/// 单次请求图像 base64 总预算（OMP `FRAME_DATA_BYTES_BUDGET`：超出会导致 provider
/// 中流 5xx；与视觉 token 预算独立——1M token 模型纸面可容纳 70 图，但请求体
/// ~11MB JSON 每轮不可承受）。
pub const FRAME_DATA_BYTES_BUDGET: usize = 3_000_000;

/// 帧数上限（按数据预算推算）。
#[must_use]
pub fn max_frames_for_data_budget(max_frame_data_bytes: usize) -> usize {
    (max_frame_data_bytes / FRAME_DATA_BYTES_ESTIMATE).max(1)
}

/// 每请求图像预算按 provider 分档（OMP `PROVIDER_IMAGE_BUDGETS`：这些 provider
/// 能承载大量图像块，其余取最严主流实测下限）。
const PROVIDER_IMAGE_BUDGETS: &[(&str, usize)] = &[
    ("anthropic", 90),
    ("amazon-bedrock", 90),
    ("openai", 200),
    ("openai-codex", 200),
    ("google", 200),
    ("google-vertex", 200),
    ("google-gemini-cli", 200),
    ("openrouter", 90),
];

/// 未知 provider 的安全下限（最严主流实测：Groq ~5）。
const DEFAULT_PROVIDER_IMAGE_BUDGET: usize = 5;

/// 每请求图像预算；未知 provider 取下限。
#[must_use]
pub fn provider_image_budget(provider: Option<&str>) -> usize {
    provider
        .and_then(|p| {
            PROVIDER_IMAGE_BUDGETS
                .iter()
                .find(|(name, _)| *name == p)
                .map(|(_, n)| *n)
        })
        .unwrap_or(DEFAULT_PROVIDER_IMAGE_BUDGET)
}

/// 单帧 token 估算（v1 统一取保守上界常数；帧高固定 1568 无按行结算）。
#[must_use]
pub const fn estimate_frame_tokens(_frame: &crate::Frame) -> usize {
    FRAME_TOKEN_ESTIMATE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shape_geometry() {
        assert_eq!(DEFAULT_SHAPE.rows(), 98);
        assert_eq!(DEFAULT_SHAPE.cols(), 196);
        assert_eq!(DEFAULT_SHAPE.capacity(), 98 * 196);
    }

    #[test]
    fn data_budget_caps_frames() {
        assert_eq!(max_frames_for_data_budget(FRAME_DATA_BYTES_BUDGET), 17);
        assert_eq!(max_frames_for_data_budget(0), 1);
    }

    #[test]
    fn provider_budgets_known_and_floor() {
        assert_eq!(provider_image_budget(Some("anthropic")), 90);
        assert_eq!(provider_image_budget(Some("openai")), 200);
        assert_eq!(provider_image_budget(Some("google")), 200);
        assert_eq!(provider_image_budget(Some("openrouter")), 90);
        // 未知 provider 落到安全下限。
        assert_eq!(provider_image_budget(Some("groq")), 5);
        assert_eq!(provider_image_budget(None), 5);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn frame_token_estimate_is_conservative() {
        assert!(FRAME_TOKEN_ESTIMATE > 4784);
    }
}
