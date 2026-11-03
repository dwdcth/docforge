//! CBZ 后端（漫画压缩包，feature = "cbz"，M5）。
//!
//! CBZ = ZIP 容器 + 每页一张图片（jpg/png/webp/…），无文本层。本后端只做
//! **字节透传**：不解码图片（解析快），每页产 `RawPage{images:[..]}` + 一个
//! Image 占位块；像素级消费（OCR / 尺寸过滤 / 装饰图判定）由
//! `ocr::images` 处理能力在解码时完成——与 DOCX/EPUB 的内嵌图管线同构。
//!
//! 页序 = 文件名**自然排序**（`page2 < page10`，ASCII 序会错）——内联实现
//! 不引 natord（算法：按数字/非数字段逐段比较）。
//!
//! 测试件策略：内存 zip 构造（命名 page1/page2/page10 验证自然序）。

use std::sync::Arc;

use crate::error::{DocError, Result};
use crate::model::{Block, BlockKind, BlockSource, Document, Page};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawImage, RawPage, RawTextItem};
use crate::registry::Registry;

/// 图片条目扩展名白名单。
const IMAGE_EXTS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "gif", "bmp"];

// ---------------------------------------------------------------------------
// 自然排序（数字段按值比较，非数字段按字典；`page2` < `page10`）
// ---------------------------------------------------------------------------

/// 把名字切成 (非数字段, 数字值) 序列供比较。
fn natural_key(name: &str) -> Vec<(String, Option<u64>)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut digits = String::new();
    for c in name.chars() {
        if c.is_ascii_digit() {
            if !buf.is_empty() {
                out.push((std::mem::take(&mut buf), None));
            }
            digits.push(c);
        } else {
            if !digits.is_empty() {
                let v = digits.parse::<u64>().unwrap_or(0);
                out.push((String::new(), Some(v)));
                digits.clear();
            }
            buf.push(c.to_ascii_lowercase());
        }
    }
    if !buf.is_empty() {
        out.push((buf, None));
    }
    if !digits.is_empty() {
        let v = digits.parse::<u64>().unwrap_or(0);
        out.push((String::new(), Some(v)));
    }
    out
}

/// 自然序比较：数字段按值、非数字段按字典；短前缀优先。
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (ka, kb) = (natural_key(a), natural_key(b));
    let mut ia = ka.iter();
    let mut ib = kb.iter();
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some((sa, va)), Some((sb, vb))) => {
                // 数字段优先按值；同为文本段再字典比较
                let ord = match (va, vb) {
                    (Some(x), Some(y)) => x.cmp(y),
                    (None, None) => sa.cmp(sb),
                    // 数字段 < 文本段（数字开头的排前，与直觉一致）
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 解析能力：zip 图片条目 → 每页一图
// ---------------------------------------------------------------------------

/// CBZ 原始读取后端。
#[derive(Default)]
pub struct CbzRawBackend;

impl ParseCapability for CbzRawBackend {
    fn id(&self) -> &str {
        "cbz.read"
    }
    fn formats(&self) -> &[&str] {
        &["cbz"]
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let file = std::fs::File::open(path)
            .map_err(|e| DocError::corrupt(format!("文件不存在或不可读 {path}: {e}")))?;
        let mut zip = zip::ZipArchive::new(file)
            .map_err(|e| DocError::corrupt(format!("CBZ 打开失败（非 ZIP）: {e}")))?;
        // 收集图片条目（目录跳过；扩展名白名单）。file_names 的条目是
        // ZipResult<Cow<str>>（UTF-8 标志位异常的条目跳过即可）
        let mut names: Vec<String> = zip
            .file_names()
            .filter_map(|n| n.ok().map(|c| c.into_owned()))
            .filter(|n| {
                !n.ends_with('/')
                    && n.rsplit_once('.')
                        .map(|(_, e)| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
                        .unwrap_or(false)
            })
            .collect();
        names.sort_by(|a, b| natural_cmp(a, b));

        let mut pages = Vec::with_capacity(names.len());
        for name in &names {
            let mut f = zip
                .by_name(name)
                .map_err(|e| DocError::corrupt(format!("CBZ 读取 {name} 失败: {e}")))?;
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut data)
                .map_err(|e| DocError::corrupt(format!("CBZ 读取 {name} 失败: {e}")))?;
            let ext = name
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            pages.push(RawPage {
                name: Some(name.clone()),
                images: vec![RawImage { data, ext, ..Default::default() }],
                // 每页一个 Image 占位块：保持 IR 块层完整（emit 的 [image]、
                // 消费者按块遍历），字节在 images 里
                items: vec![RawTextItem {
                    image: Some(RawImage::default()),
                    source: BlockSource::Native,
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
        if pages.is_empty() {
            return Err(DocError::corrupt("CBZ 内无图片条目"));
        }
        Ok(RawDoc { pages, ..Default::default() })
    }
}

// ---------------------------------------------------------------------------
// 插件：每页一个 Image 块
// ---------------------------------------------------------------------------

/// CBZ 插件：RawDoc（每页一图）→ IR（Image 块 + Page.images）。
pub struct CbzPlugin;

impl FormatPlugin for CbzPlugin {
    fn id(&self) -> &str {
        "cbz"
    }
    fn formats(&self) -> &[&str] {
        &["cbz"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["cbz.read"]
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        // opt-in：注册 ocr-images 能力后整页图进 OCR（漫画/扫描册场景）
        vec!["ocr-images"]
    }
    fn assemble(&self, raw: &RawDoc) -> Document {
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
                        // ocr_text 必须透传（ocr-images 链的产出落点）——
                        // 图像直读工人交叉发现 cbz 曾在此丢弃（M5 修复）
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

/// 注册 CBZ 后端（解析能力 + 插件）。
pub fn register_defaults(reg: &mut Registry) {
    let _ = reg.register_parse(Arc::new(CbzRawBackend), None);
    reg.register_plugin(Arc::new(CbzPlugin));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 构造 CBZ：给定 (文件名, PNG 尺寸) 列表。
    fn build_cbz(dir_name: &str, names: &[&str]) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("cbz-{dir_name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("book.cbz");
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        let mut img = image::RgbaImage::new(4, 4);
        img.put_pixel(0, 0, image::Rgba([1, 2, 3, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        for n in names {
            zw.start_file(*n, opts).unwrap();
            zw.write_all(&png).unwrap();
        }
        zw.finish().unwrap();
        path
    }

    #[test]
    fn natural_order_pages() {
        let p = build_cbz("order", &["page10.png", "page2.png", "page1.png", "cover.jpg"]);
        let raw = CbzRawBackend
            .parse(p.to_str().unwrap())
            .unwrap();
        let names: Vec<&str> = raw.pages.iter().map(|p| p.name.as_deref().unwrap()).collect();
        // 自然序：cover（字母段在前无数字）→ page1 → page2 → page10
        assert_eq!(names, vec!["cover.jpg", "page1.png", "page2.png", "page10.png"], "{names:?}");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn assemble_one_image_block_per_page() {
        let p = build_cbz("asm", &["a.png", "b.png"]);
        let raw = CbzRawBackend.parse(p.to_str().unwrap()).unwrap();
        let doc = CbzPlugin.assemble(&raw);
        assert_eq!(doc.pages.len(), 2);
        for page in &doc.pages {
            assert_eq!(page.blocks.len(), 1);
            assert_eq!(page.blocks[0].kind, BlockKind::Image);
            assert_eq!(page.images.len(), 1);
            assert!(!page.images[0].data.is_empty());
        }
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn empty_archive_is_corrupt() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("cbz-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.cbz");
        let f = std::fs::File::create(&path).unwrap();
        zip::ZipWriter::new(f).finish().unwrap();
        let err = CbzRawBackend.parse(path.to_str().unwrap()).unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(err.message.contains("无图片"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn natural_cmp_unit() {
        assert_eq!(natural_cmp("page2", "page10"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("page10", "page9"), std::cmp::Ordering::Greater);
        assert_eq!(natural_cmp("a1b2", "a1b2"), std::cmp::Ordering::Equal);
        assert_eq!(natural_cmp("img", "img2"), std::cmp::Ordering::Less);
    }
}
