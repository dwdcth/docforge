//! 内嵌图片 OCR 处理能力（feature = "ocr"，M3）。
//!
//! docparser 的处理能力语义（docparse-plan §8）：解析能力把内嵌图片字节放进
//! `RawPage.images`（DOCX `ocr_images` 开关 / EPUB 五件套），本能力就地增强——
//! 解码 → `OcrEngine` 识别 → 文本回填 `RawImage.ocr_text`（IR 的
//! `Image.ocr_text` / `image_text_length` 统计由此而来）。
//!
//! 容错：单图解码/识别失败不致命（记 warnings），装饰图（px < `min_px`）跳过
//!（对齐 docparser 的 50px 口径），已有 ocr_text 的不重复识别。

use std::sync::Arc;

use crate::error::Result;
use crate::ocr::OcrEngine;
use crate::plugin::{ProcessCapability, RawDoc};
use crate::registry::Registry;

/// 内嵌图片 OCR 处理能力。
pub struct ImageOcrCapability {
    pub engine: Arc<dyn OcrEngine>,
    /// 最小像素边长（宽或高小于此值视为装饰图跳过；默认 50，对齐 docparser）。
    pub min_px: u32,
}

impl ImageOcrCapability {
    pub fn new(engine: Arc<dyn OcrEngine>) -> Self {
        Self { engine, min_px: 50 }
    }

    /// 自定义装饰图阈值。
    pub fn with_min_px(mut self, min_px: u32) -> Self {
        self.min_px = min_px;
        self
    }
}

impl ProcessCapability for ImageOcrCapability {
    fn id(&self) -> &str {
        "ocr-images"
    }

    fn should_apply(&self, raw: &RawDoc) -> bool {
        raw.pages.iter().any(|p| p.images.iter().any(|im| im.ocr_text.is_none()))
    }

    fn process(&self, raw: &mut RawDoc) -> Result<()> {
        let mut warnings: Vec<String> = Vec::new();
        for (p_idx, page) in raw.pages.iter_mut().enumerate() {
            for img in page.images.iter_mut() {
                if img.ocr_text.as_deref().is_some_and(|t| !t.trim().is_empty()) {
                    continue; // 已有识别结果（后端自带）不重复
                }
                if let Some((w, h)) = img.px {
                    if w < self.min_px || h < self.min_px {
                        continue; // 装饰图
                    }
                }
                let decoded = match image::load_from_memory(&img.data) {
                    Ok(d) => d.to_rgba8(),
                    Err(_) => {
                        warnings.push(format!("图片解码失败：第 {} 页 {} 字节", p_idx + 1, img.data.len()));
                        continue;
                    }
                };
                match self.engine.recognize(&decoded) {
                    Ok(lines) if !lines.is_empty() => {
                        img.ocr_text = Some(
                            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"),
                        );
                    }
                    Ok(_) => {} // 无文字：合法静默
                    Err(e) => warnings.push(format!("图片 OCR 失败：{e}")),
                }
            }
        }
        if !warnings.is_empty() {
            let joined = warnings.join("; ");
            raw.meta
                .insert("warnings".to_string(), format!("{}{}", raw.meta.get("warnings").map(|w| format!("{w}; ")).unwrap_or_default(), joined));
        }
        Ok(())
    }
}

/// 注册图片 OCR 处理能力。**不自动挂链**：需格式插件的
/// `default_process_chain` 含 `"ocr-images"` 才生效（消费者显式装配，
/// 对齐 docparser opt-in 语义）。
pub fn register(reg: &mut Registry, engine: Arc<dyn OcrEngine>) {
    reg.register_process(Arc::new(ImageOcrCapability::new(engine)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{RawImage, RawPage};

    struct MockEngine(&'static str);
    impl OcrEngine for MockEngine {
        fn name(&self) -> &str {
            self.0
        }
        fn recognize(
            &self,
            _image: &image::RgbaImage,
        ) -> std::result::Result<Vec<crate::ocr::RecognizedLine>, String> {
            Ok(vec![crate::ocr::RecognizedLine {
                text: "识别结果行".into(),
                score: 0.9,
                quad: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            }])
        }
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::new(w, h);
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    fn raw_with_images(images: Vec<RawImage>) -> RawDoc {
        RawDoc {
            pages: vec![RawPage { images, ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn fills_ocr_text_for_decodable_images() {
        let mut raw = raw_with_images(vec![RawImage {
            data: png_bytes(100, 80),
            ..Default::default()
        }]);
        ImageOcrCapability::new(Arc::new(MockEngine("mock")))
            .process(&mut raw)
            .unwrap();
        assert_eq!(raw.pages[0].images[0].ocr_text.as_deref(), Some("识别结果行"));
    }

    #[test]
    fn skips_tiny_and_existing() {
        let mut raw = raw_with_images(vec![
            RawImage {
                data: png_bytes(10, 10),
                px: Some((10, 10)), // 装饰图：px 已知且 < 50
                ..Default::default()
            },
            RawImage {
                data: png_bytes(100, 100),
                ocr_text: Some("已有".into()),
                ..Default::default()
            },
        ]);
        ImageOcrCapability::new(Arc::new(MockEngine("mock")))
            .process(&mut raw)
            .unwrap();
        assert_eq!(raw.pages[0].images[0].ocr_text, None, "装饰图跳过");
        assert_eq!(raw.pages[0].images[1].ocr_text.as_deref(), Some("已有"), "已有不覆盖");
    }

    #[test]
    fn undecodable_image_records_warning_not_fatal() {
        let mut raw = raw_with_images(vec![RawImage {
            data: vec![0x00, 0x01, 0x02], // 非图像字节
            ..Default::default()
        }]);
        ImageOcrCapability::new(Arc::new(MockEngine("mock")))
            .process(&mut raw)
            .unwrap();
        assert!(raw.pages[0].images[0].ocr_text.is_none());
        assert!(raw.meta.get("warnings").is_some_and(|w| w.contains("解码失败")));
    }
}
