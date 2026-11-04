//! OCR 引擎抽象：整页图像 → 识别行。
//!
//! 注入式设计（docparse-plan §8）：pdf 后端接受 `Arc<dyn OcrEngine>`，
//! - rumi/上层可注入自有引擎实例（避免模型双重加载）；
//! - 测试可注入 mock（金测不依赖模型缓存）；
//! - 其它引擎（Paddle/Tesseract 绑定）无需改 pdf 后端即可接入。
//!
//! 批量入口：[`OcrEngine::recognize_batch`] 带默认实现（逐图循环
//! recognize），引擎可覆写吃批红利（GPU batch inference）；pdf 后端经
//! `PdfOptions::ocr_batch` 消费（吞吐 vs 首页延迟旋钮）。
//!
//! `ocr-oar` feature 提供 oar-ocr 默认实现（`oar` 模块）。

pub mod images;

#[cfg(feature = "ocr-oar")]
pub mod oar;

use crate::model::Rect;

/// OCR 识别行（引擎输出）。
///
/// 四点框保真旋转/倾斜（PaddleOCR det polys 天然四点），像素坐标、左上原点。
#[derive(Debug, Clone, PartialEq)]
pub struct RecognizedLine {
    pub text: String,
    /// 识别置信度 0..1
    pub score: f32,
    /// 四点框，顺时针自左上：[[x, y]; 4]
    pub quad: [[f32; 2]; 4],
}

impl RecognizedLine {
    /// 四点框的轴对齐包围盒（`Block.bbox` 用；`Page.coord_units = Px` 语义）。
    pub fn axis_rect(&self) -> Rect {
        let mut x0 = f32::MAX;
        let mut y0 = f32::MAX;
        let mut x1 = f32::NEG_INFINITY;
        let mut y1 = f32::NEG_INFINITY;
        for &[x, y] in &self.quad {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        Rect { x0, y0, x1, y1 }
    }
}

/// OCR 引擎：整页 RGBA 图像 → 识别行集合（行序即引擎输出序）。
///
/// 实现须 `Send + Sync`（docforge 全同步 API，引擎内部自行串行化 session 访问）。
/// 批量吞吐见 [`Self::recognize_batch`]（默认实现，可不覆写）。
pub trait OcrEngine: Send + Sync {
    /// 引擎名（诊断/日志用，如 "oar-ocr pp-ocrv6-small"）。
    fn name(&self) -> &str;

    /// 识别一页。空文本页返回空 Vec；错误返回可读消息（pdf 后端包装为
    /// `DocError` 后按非终止码回退链处理）。
    fn recognize(
        &self,
        image: &image::RgbaImage,
    ) -> std::result::Result<Vec<RecognizedLine>, String>;

    /// 批量识别（默认实现 = 逐图调 [`Self::recognize`]，首个错误即返回）。
    /// 引擎侧可覆写吃批红利（GPU batch inference：oar/Paddle 类引擎一次
    /// 前向多图，吞吐常显著高于逐图）。`images` 与返回的 Vec 按位对应。
    ///
    /// # 契约
    /// 覆写侧必须与逐图调用结果一致——同一图在批内/批外产出相同行集（默认
    /// 实现天然满足）；pdf 后端按此契约把批结果按位组装回各页，不做批间
    /// 混排，逐页等价性由此保证。
    fn recognize_batch(
        &self,
        images: &[&image::RgbaImage],
    ) -> std::result::Result<Vec<Vec<RecognizedLine>>, String> {
        images.iter().map(|im| self.recognize(im)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_rect_covers_all_four_points() {
        let line = RecognizedLine {
            text: "斜行".into(),
            score: 0.9,
            quad: [[10.0, 5.0], [20.0, 6.0], [19.0, 15.0], [9.0, 14.0]],
        };
        let r = line.axis_rect();
        assert_eq!(r.x0, 9.0);
        assert_eq!(r.y0, 5.0);
        assert_eq!(r.x1, 20.0);
        assert_eq!(r.y1, 15.0);
    }
}
