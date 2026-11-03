//! oar-ocr 默认引擎实现（feature = "ocr-oar"）。
//!
//! PP-OCRv6 small 文字管线（det+rec，auto-download 注册表 ModelScope → `~/.oar`），
//! 懒加载单例：首次 [`OarOcrEngine::recognize`] 触发构建（首次含模型下载）。
//! `Mutex<Option<..>>` 而非裸 OnceLock：懒构建可能失败（断网），且串行化
//! 对同一 ONNX session 的 predict 调用。

use std::sync::{Mutex, MutexGuard, OnceLock};

use image::{DynamicImage, RgbImage};
use oar_ocr::prelude::*;
use oar_ocr::processors::BoundingBox;

use super::{OcrEngine, RecognizedLine};

/// oar-ocr 文字管线引擎（全局单例包装）。
pub struct OarOcrEngine {
    inner: Mutex<Option<OAROCR>>,
}

impl OarOcrEngine {
    /// 全局共享实例（`Arc<dyn OcrEngine>` 形态，直接注入 `PdfOptions::engine`；
    /// 不构建，真正构建发生在首次 recognize）。
    pub fn shared() -> std::sync::Arc<dyn OcrEngine> {
        static SHARED: OnceLock<std::sync::Arc<OarOcrEngine>> = OnceLock::new();
        SHARED
            .get_or_init(|| {
                std::sync::Arc::new(OarOcrEngine {
                    inner: Mutex::new(None),
                })
            })
            .clone()
    }

    /// 预热：构建 ONNX session（幂等、可重试；首次含模型下载）。
    pub fn warmup(&self) -> Result<(), String> {
        self.lock_engine().map(|_| ())
    }

    /// 取引擎锁；未构建则当场构建。
    fn lock_engine(&self) -> Result<MutexGuard<'_, Option<OAROCR>>, String> {
        let mut guard = self.inner.lock().map_err(|_| "引擎锁中毒".to_string())?;
        if guard.is_none() {
            *guard = Some(
                OAROCRBuilder::pp_ocrv6(PpOcrV6Size::Small)
                    .build()
                    .map_err(|e| format!("oar-ocr 引擎构建失败：{e}"))?,
            );
        }
        Ok(guard)
    }
}

impl OcrEngine for OarOcrEngine {
    fn name(&self) -> &str {
        "oar-ocr pp-ocrv6-small"
    }

    fn recognize(
        &self,
        image: &image::RgbaImage,
    ) -> std::result::Result<Vec<RecognizedLine>, String> {
        let guard = self.lock_engine()?;
        let engine = guard.as_ref().expect("lock_engine 保证已初始化");
        let rgb: RgbImage = DynamicImage::ImageRgba8(image.clone()).into_rgb8();
        let results = engine
            .predict(vec![rgb])
            .map_err(|e| format!("oar-ocr 推理失败：{e}"))?;
        let result = results
            .into_iter()
            .next()
            .ok_or_else(|| "oar-ocr predict 返回空结果集".to_string())?;
        Ok(result
            .text_regions
            .iter()
            .filter_map(|tr| {
                let (text, score) = tr.text_with_confidence()?;
                Some(RecognizedLine {
                    text: text.to_string(),
                    score,
                    quad: quad_of(&tr.bounding_box),
                })
            })
            .collect())
    }
}

/// oar `BoundingBox.points` → 固定四点框。
///
/// 检测输出通常是 4 点多边形（PaddleOCR det polys）；不足 4 点按最小外接
/// 矩形补齐，空框返回全 0。
fn quad_of(bb: &BoundingBox) -> [[f32; 2]; 4] {
    if bb.points.len() >= 4 {
        [
            [bb.points[0].x, bb.points[0].y],
            [bb.points[1].x, bb.points[1].y],
            [bb.points[2].x, bb.points[2].y],
            [bb.points[3].x, bb.points[3].y],
        ]
    } else if bb.points.is_empty() {
        [[0.0, 0.0]; 4]
    } else {
        let (x0, y0, x1, y1) = (bb.x_min(), bb.y_min(), bb.x_max(), bb.y_max());
        [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
    }
}
