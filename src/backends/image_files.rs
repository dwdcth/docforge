//! 图像直读后端（png/jpg/webp/bmp/tiff 等单图文件，feature = "image-files"）。
//!
//! 场景：扫描件、截图——**一张图就是一个文档**。设计完全复用 CBZ 的
//! "字节透传"模式（见 `backends/cbz.rs`）：不解码像素（解析快、内存省），
//! 每文件产单页 `RawPage{images:[..]}` + 一个 Image 占位块；像素级消费
//! （OCR / 装饰图判定）由 `ocr::images` 处理能力在解码时完成。复用理由：
//! 页结构（images + Image 占位块）与消费位（ocr-images 链）同构，仅容器
//! 从 ZIP 换成裸文件——单文件单页，无页序问题（页名 = 文件名）。
//!
//! 本格式的核心价值即 `default_process_chain = ["ocr-images"]`：图 → OCR →
//! `Image.ocr_text` 进 IR，docforge-cli 一条命令完成"图→文"。
//!
//! 与 CBZ 的差异：像素尺寸（px）不解码拿不到 → None（整图即文档，装饰图
//! 过滤对它无意义，交给 OCR 链解码后自行判断）。
//!
//! 测试件策略：image crate 内存编码 PNG/JPEG 落 `target/` 后按扩展名命名。

use std::sync::Arc;

use crate::error::{DocError, ErrorCode, Result};
use crate::model::{Block, BlockKind, BlockSource, Document, Page};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawImage, RawPage, RawTextItem};
use crate::registry::Registry;

// ---------------------------------------------------------------------------
// 解析能力：单图文件 → 单页（字节透传，不解码）
// ---------------------------------------------------------------------------

/// 图像直读原始后端。
#[derive(Default)]
pub struct ImageFilesRawBackend;

impl ParseCapability for ImageFilesRawBackend {
    fn id(&self) -> &str {
        "image-files.read"
    }
    // jpg/jpeg、tif/tiff 是同一格式的两种扩展名，全部声明让 router 按扩展名直连
    fn formats(&self) -> &[&str] {
        &["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff"]
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        // 预检走终止码 Unsupported：文件不存在是确定性结论，不能被兜底后端掩盖
        //（对齐 pdf.rs）
        let p = std::path::Path::new(path);
        if !p.is_file() {
            return Err(DocError {
                code: ErrorCode::Unsupported,
                message: format!("文件不存在：{}", p.display()),
                attempts: vec![],
            });
        }
        let data = std::fs::read(p)
            .map_err(|e| DocError::corrupt(format!("图片读取失败 {path}: {e}")))?;
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        Ok(RawDoc {
            // 单文件 = 单页：字节透传不解码，px 未知（None），OCR 链解码时
            // 自行判断
            pages: vec![RawPage {
                name: Some(name),
                images: vec![RawImage { data, ext, ..Default::default() }],
                // 一个 Image 占位块：保持 IR 块层完整（emit 的 [image]、
                // 消费者按块遍历），字节在 images 里（对齐 cbz.rs）
                items: vec![RawTextItem {
                    image: Some(RawImage::default()),
                    source: BlockSource::Native,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        })
    }
}

// ---------------------------------------------------------------------------
// 插件：单页一个 Image 块
// ---------------------------------------------------------------------------

/// 图像直读插件：RawDoc（单页一图）→ IR（Image 块 + Page.images）。
pub struct ImageFilesPlugin;

impl FormatPlugin for ImageFilesPlugin {
    fn id(&self) -> &str {
        "image-files"
    }
    fn formats(&self) -> &[&str] {
        &["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["image-files.read"]
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        // 图→文核心链：整图进 OCR（扫描件场景）。未注册 ocr-images 能力时
        // 该链为空，仅出 Image 块 + Page.images（字节仍可从 IR 取）
        vec!["ocr-images"]
    }
    fn assemble(&self, raw: &RawDoc) -> Document {
        // 结构照抄 cbz.rs 的 assemble。差异一处：`ocr_text` 从 RawImage 透传
        // 到 IR `Image`——本格式 OCR 文本就是文档正文，丢弃即失去图→文语义
        //（cbz.rs 的转换未带 ocr_text，其消费位在页文本而非图片正文）
        let pages = raw
            .pages
            .iter()
            .map(|rp| {
                // RawImage（plugin 层）→ Image（IR 层）：字节与 ext 语义保留
                let images: Vec<crate::model::Image> = rp
                    .images
                    .iter()
                    .map(|im| crate::model::Image {
                        data: im.data.clone(),
                        bbox: im.bbox,
                        ocr_text: im.ocr_text.clone(),
                        ..Default::default()
                    })
                    .collect();
                Page {
                    name: rp.name.clone(),
                    blocks: vec![Block {
                        kind: BlockKind::Image,
                        source: BlockSource::Native,
                        ..Default::default()
                    }],
                    images,
                    ..Default::default()
                }
            })
            .collect();
        Document { pages, ..Default::default() }
    }
}

/// 注册图像直读后端（解析能力 + 插件）。
pub fn register_defaults(reg: &mut Registry) {
    let _ = reg.register_parse(Arc::new(ImageFilesRawBackend), None);
    reg.register_plugin(Arc::new(ImageFilesPlugin));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::RecognizedLine;
    use crate::registry::Registry;

    /// 内存编码一张"扫描件样张"（白底 + 深灰条带，手法对齐 pdf.rs sample）
    /// 并按扩展名落 `target/`，返回文件路径与编码字节。
    fn make_image_file(
        dir_name: &str,
        file_name: &str,
        fmt: image::ImageFormat,
    ) -> (std::path::PathBuf, Vec<u8>) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("imgfile-{dir_name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut img = image::RgbImage::new(200, 120);
        for (_, _, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([248, 248, 244]);
        }
        for y in 30..50 {
            for x in 20..180 {
                img.put_pixel(x, y, image::Rgb([40, 40, 40]));
            }
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), fmt)
            .unwrap();
        let path = dir.join(file_name);
        std::fs::write(&path, &bytes).unwrap();
        (path, bytes)
    }

    fn cleanup(p: &std::path::Path) {
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn single_png_becomes_one_page() {
        let (p, bytes) = make_image_file("png", "scan.png", image::ImageFormat::Png);
        let raw = ImageFilesRawBackend.parse(p.to_str().unwrap()).unwrap();
        assert_eq!(raw.pages.len(), 1);
        let page = &raw.pages[0];
        assert_eq!(page.name.as_deref(), Some("scan.png"), "页名 = 文件名");
        assert_eq!(page.images.len(), 1);
        let im = &page.images[0];
        assert_eq!(im.data, bytes, "字节透传：不解码、不重编码");
        assert_eq!(im.ext, "png");
        assert_eq!(im.px, None, "不解码拿不到像素尺寸");
        assert_eq!(im.ocr_text, None);
        // Image 占位块：块层完整，字节在 images 里
        assert_eq!(page.items.len(), 1);
        assert!(page.items[0].image.is_some());
        assert_eq!(page.items[0].source, BlockSource::Native);
        assert!(page.items[0].text.is_empty());
        cleanup(&p);
    }

    #[test]
    fn jpeg_extension_mapped() {
        // jpg / jpeg 是同一格式的两种扩展名：各自成格式键，ext 原样保留
        for (file, want_ext) in [("photo.jpg", "jpg"), ("photo.jpeg", "jpeg")] {
            let (p, bytes) =
                make_image_file("jpeg", file, image::ImageFormat::Jpeg);
            let raw = ImageFilesRawBackend.parse(p.to_str().unwrap()).unwrap();
            assert_eq!(raw.pages.len(), 1);
            assert_eq!(raw.pages[0].name.as_deref(), Some(file));
            assert_eq!(raw.pages[0].images[0].ext, want_ext, "{file}");
            assert_eq!(raw.pages[0].images[0].data, bytes);
            cleanup(&p);
        }
    }

    /// 端到端：图 → ocr-images 链（Mock 引擎）→ Image.ocr_text 进 IR。
    /// 装配形态对齐 `ocr/images.rs` 测试（MockEngine + Registry 注册），
    /// 走 `DocumentEngine::parse` 全流程（sniff → 解析链 → 处理链 → assemble）。
    #[test]
    fn assemble_exposes_image_for_ocr_chain() {
        struct MockEngine;
        impl crate::ocr::OcrEngine for MockEngine {
            fn name(&self) -> &str {
                "mock"
            }
            fn recognize(
                &self,
                _image: &image::RgbaImage,
            ) -> std::result::Result<Vec<RecognizedLine>, String> {
                Ok(vec![RecognizedLine {
                    text: "识别结果行".into(),
                    score: 0.9,
                    quad: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                }])
            }
        }
        let mut reg = Registry::new();
        register_defaults(&mut reg);
        crate::ocr::images::register(&mut reg, Arc::new(MockEngine));

        let (p, _bytes) = make_image_file("e2e", "scan.png", image::ImageFormat::Png);
        let doc = crate::DocumentEngine::new(reg)
            .parse(p.to_str().unwrap())
            .unwrap();
        cleanup(&p);
        assert_eq!(doc.meta.file_type, "png");
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].blocks.len(), 1);
        assert_eq!(doc.pages[0].blocks[0].kind, BlockKind::Image);
        assert_eq!(
            doc.pages[0].images[0].ocr_text.as_deref(),
            Some("识别结果行"),
            "OCR 文本进 IR"
        );
        assert_eq!(doc.meta.image_text_length, 5, "finalize 统计 OCR 正文");
    }

    #[test]
    fn missing_file_is_unsupported() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("imgfile-missing-{}", std::process::id()));
        let ghost = dir.join("ghost.png");
        let err = ImageFilesRawBackend
            .parse(ghost.to_str().unwrap())
            .unwrap_err();
        // 文件不存在是确定性结论：终止码 Unsupported（对齐 pdf.rs 文案）
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert!(err.message.contains("文件不存在"), "实际：{}", err.message);
    }
}
