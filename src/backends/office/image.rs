// 吸收自 docparser-office 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：crate 引用重写（docparser_core→docforge、office 内部→super）。
//! OOXML 内嵌位图提取（与 Python `plugins/ooxml.py` 的图片工具同口径）。
//!
//! Python 侧是 `_OCRABLE_EXT` / `_MIN_IMG_PX` / `_img_px` / `_make_image` /
//! `_blip_rids` 五个小函数；这里一一对应，供 docx / xlsx / pptx 三个后端共用，
//! 保证「哪些图算图、多大算装饰、去重键是什么」三处只有一份实现。
//!
//! 默认关闭（`OfficeOptions::with_ocr_images`）：读图要解压位图字节，属于 opt-in。

use crate::plugin::{RawImage, RawTextItem};

/// 可 OCR 的光栅图扩展名白名单（对齐 Python `_OCRABLE_EXT`）：
/// EMF/WMF/SmartArt 这类矢量图**明确跳过**，不白跑 OCR。
pub(crate) const OCRABLE_EXT: [&str; 8] =
    ["png", "jpg", "jpeg", "gif", "bmp", "tiff", "tif", "webp"];

/// 装饰图过滤阈值：`min(w, h) < 50` 视为图标/水印（对齐 Python `_MIN_IMG_PX`）。
pub(crate) const MIN_IMG_PX: u32 = 50;

/// 部件扩展名（小写、去点）：`word/media/img1.PNG` → `png`（对齐 Python `_ext_of`）。
pub(crate) fn ext_of(part: &str) -> String {
    part.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// `ext` 是否在可 OCR 白名单内。
pub(crate) fn ocrable_ext(ext: &str) -> bool {
    OCRABLE_EXT.contains(&ext)
}

/// 从文件头解析像素尺寸（宽, 高）；不支持的格式返回 `(0, 0)` = 未知，
/// **不因此过滤**（对齐 Python `_img_px`）。
///
/// 支持 PNG / JPEG / GIF / BMP / WebP(VP8X)；TIFF 等未解析（同 Python）。
pub(crate) fn img_px(data: &[u8]) -> (u32, u32) {
    // PNG：IHDR 里的宽高（大端 4 字节，偏移 16 / 20）
    if data.len() >= 24 && &data[..8] == b"\x89PNG\r\n\x1a\n" {
        return (be_u32(&data[16..20]), be_u32(&data[20..24]));
    }
    // JPEG：扫段找 SOF（C0–CF，跳过 DHT/JPG 扩展等），宽在 i+7、高在 i+5
    if data.len() >= 2 && data[..2] == [0xFF, 0xD8] {
        let n = data.len();
        let mut i = 2usize;
        while i + 9 < n {
            if data[i] != 0xFF {
                i += 1;
                continue;
            }
            let m = data[i + 1];
            if matches!(
                m,
                0xC0 | 0xC1
                    | 0xC2
                    | 0xC3
                    | 0xC5
                    | 0xC6
                    | 0xC7
                    | 0xC9
                    | 0xCA
                    | 0xCB
                    | 0xCD
                    | 0xCE
                    | 0xCF
            ) {
                return (be_u16(&data[i + 7..i + 9]), be_u16(&data[i + 5..i + 7]));
            }
            // RSTn / SOI / EOI 无长度字段，直接跳过 2 字节
            if m == 0xD8 || m == 0xD9 || (0xD0..=0xD7).contains(&m) {
                i += 2;
                continue;
            }
            // 其余段：2 字节标记 + 2 字节长度（长度含自身）
            i += 2 + be_u16(&data[i + 2..i + 4]) as usize;
        }
        return (0, 0);
    }
    // GIF：逻辑屏幕宽高（小端 2 字节，偏移 6 / 8）
    if data.len() >= 10 && &data[..3] == b"GIF" {
        return (le_u16(&data[6..8]) as u32, le_u16(&data[8..10]) as u32);
    }
    // BMP：DIB 头里的宽高（小端有符号 4 字节，偏移 18 / 22；取绝对值兼容自上而下位图）
    if data.len() >= 26 && &data[..2] == b"BM" {
        return (
            le_i32(&data[18..22]).unsigned_abs(),
            le_i32(&data[22..26]).unsigned_abs(),
        );
    }
    // WebP(VP8X)：canvas 宽高各 3 字节，存的是「实际值 - 1」
    if data.len() >= 30
        && &data[..4] == b"RIFF"
        && &data[8..12] == b"WEBP"
        && &data[12..16] == b"VP8X"
    {
        return (1 + le_u24(&data[24..27]), 1 + le_u24(&data[27..30]));
    }
    (0, 0)
}

fn be_u16(b: &[u8]) -> u32 {
    u16::from_be_bytes([b[0], b[1]]) as u32
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le_u24(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], 0])
}

fn le_i32(b: &[u8]) -> i32 {
    i32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// 去重键：md5 十六进制（与 Python `hashlib.md5(data).hexdigest()` 同值）。
pub(crate) fn dedup_key(data: &[u8]) -> String {
    use md5::Digest as _;
    format!("{:x}", md5::Md5::digest(data))
}

/// 读 `part` 并构造 `RawImage`；不满足白名单/空字节/过小一律 `None`
/// （对齐 Python `_make_image`，含 `want_bytes=True` 分支）。
pub(crate) fn make_image(zip: &mut zip::ZipArchive<std::fs::File>, part: &str) -> Option<RawImage> {
    let ext = ext_of(part);
    if !ocrable_ext(&ext) {
        return None;
    }
    let data = super::zip_read_bytes(zip, part)?;
    if data.is_empty() {
        return None;
    }
    let (w, h) = img_px(&data);
    if w != 0 && h != 0 && w.min(h) < MIN_IMG_PX {
        return None;
    }
    Some(RawImage {
        data,
        bbox: None,
        ext,
        px: if w != 0 && h != 0 { Some((w, h)) } else { None },
        ocr_text: None,
    })
}

/// 图片项：`text` 为空、`image` 置位——`assemble` 据此产出 `BlockKind::Image`
/// （对齐 Python `items.append(("image", im))`）。
/// 调用点只在 docx/pptx 提取路径（xlsx 的图走 `Page.images`）——二者单开时
/// 调用点归零，允许 dead（与 stream.rs 的 `unsupported` 同款处理）。
#[allow(dead_code)]
pub(crate) fn image_item(im: RawImage) -> RawTextItem {
    RawTextItem {
        image: Some(im),
        ..Default::default()
    }
}

/// 图片读取器：持有 ZIP 与「已见过的部件名」，一次构造对应一个去重域
/// （docx = 整篇，pptx = 每页，xlsx = 每张 sheet 的每个 drawing）。
///
/// 去重**先于**构造：同一部件不会被二次读取，且「太小的图」也占名额
/// （对齐 Python `media_seen.add(part)` 在 `_make_image` 之前的顺序）。
/// docx/pptx 单开时调用点归零，允许 dead。
#[allow(dead_code)]
pub(crate) struct ImageReader<'a> {
    zip: &'a mut zip::ZipArchive<std::fs::File>,
    seen: Vec<String>,
}

#[allow(dead_code)]
impl<'a> ImageReader<'a> {
    pub(crate) fn new(zip: &'a mut zip::ZipArchive<std::fs::File>) -> Self {
        Self {
            zip,
            seen: Vec::new(),
        }
    }

    /// 读取并构造 `part` 对应的图；已读过 / 命中过滤（扩展名、空字节、过小）→ `None`。
    pub(crate) fn load(&mut self, part: &str) -> Option<RawImage> {
        if self.seen.iter().any(|s| s == part) {
            return None;
        }
        self.seen.push(part.to_string());
        make_image(self.zip, part)
    }
}

/// 当前元素若是图片引用节点（`a:blip` / `v:imagedata`），返回其关系 id。
///
/// 只认 `r:embed`（a:blip 内嵌图）与 `r:id`（v:imagedata 内嵌图）：`r:link`
/// 指向外部文件，Python 同样不收。属性按**本地名**匹配，前缀无所谓。
pub(crate) fn rid_of_image_node(e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    let local = e.name().local_name();
    let name = local.as_ref();
    if name != super::tags::A_BLIP && name != super::tags::VML_IMAGEDATA {
        return None;
    }
    let mut rid: Option<String> = None;
    for attr in e.attributes().filter_map(|a| a.ok()) {
        match attr.key.local_name().as_ref() {
            "embed" | "id" => {
                let value = attr.value.to_string();
                if !value.is_empty() {
                    rid = Some(value);
                }
            }
            _ => {}
        }
    }
    rid
}

/// 整段 XML 里的图片引用 id（文档序、**同一元素内去重**）：
/// 用于 drawing 部件这种「一次读完、不需要与文本交错」的场景
/// （对齐 Python `_blip_rids(el)` 的 `v not in rids` 语义）。
pub(crate) fn blip_rids(xml: &str) -> Vec<String> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<String> = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if let Some(rid) = rid_of_image_node(e) {
                    if !out.contains(&rid) {
                        out.push(rid);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0, 0, 0, 13]); // IHDR 长度
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&height.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    #[test]
    fn ext_of_lowercases_without_dot() {
        assert_eq!(ext_of("word/media/img1.PNG"), "png");
        assert_eq!(ext_of("ppt/media/pic.jpeg"), "jpeg");
        assert_eq!(ext_of("word/media/noext"), "");
        assert!(ocrable_ext("tif"));
        assert!(!ocrable_ext("emf"));
    }

    #[test]
    fn img_px_reads_png_and_gif() {
        assert_eq!(img_px(&png(120, 80)), (120, 80));
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&64u16.to_le_bytes());
        gif.extend_from_slice(&48u16.to_le_bytes());
        gif.extend_from_slice(&[0, 0, 0]);
        assert_eq!(img_px(&gif), (64, 48));
    }

    #[test]
    fn img_px_reads_bmp_and_webp() {
        // BMP：文件头 14 字节 + DIB 头，宽高在偏移 18 / 22（小端有符号）
        let mut bmp = vec![0u8; 54];
        bmp[..2].copy_from_slice(b"BM");
        bmp[18..22].copy_from_slice(&(-100i32).to_le_bytes());
        bmp[22..26].copy_from_slice(&60i32.to_le_bytes());
        assert_eq!(img_px(&bmp), (100, 60));

        // WebP(VP8X)：宽高各 3 字节小端，存值 = 实际 - 1
        let mut webp = vec![0u8; 30];
        webp[..4].copy_from_slice(b"RIFF");
        webp[8..12].copy_from_slice(b"WEBP");
        webp[12..16].copy_from_slice(b"VP8X");
        webp[24..27].copy_from_slice(&[0x3F, 0x01, 0x00]); // 319 → 320
        webp[27..30].copy_from_slice(&[0x8F, 0x00, 0x00]); // 143 → 144
        assert_eq!(img_px(&webp), (320, 144));
    }

    #[test]
    fn img_px_reads_jpeg_sof_and_skips_length_segments() {
        // SOI + APP0(len=4, 负载 2 字节) + SOF0(len=11, 精度/高/宽)
        let mut jpg = vec![0xFF, 0xD8];
        jpg.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        jpg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
        jpg.extend_from_slice(&72u16.to_be_bytes()); // 高
        jpg.extend_from_slice(&96u16.to_be_bytes()); // 宽
        jpg.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        assert_eq!(img_px(&jpg), (96, 72));
        // 残缺数据：不 panic，返回未知
        assert_eq!(img_px(&[0xFF, 0xD8, 0x00]), (0, 0));
        assert_eq!(img_px(b"not an image"), (0, 0));
    }

    #[test]
    fn dedup_key_is_md5_hex() {
        assert_eq!(dedup_key(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_ne!(dedup_key(b"abc"), dedup_key(b"abd"));
    }

    #[test]
    fn blip_rids_collects_embed_and_vml_id_in_order() {
        let xml = concat!(
            r#"<root><a:blip r:embed="rId2"/>"#,
            r#"<v:imagedata r:id="rId3"/>"#,
            r#"<a:blip r:embed="rId2"/>"#,
            r#"<a:blip r:link="rId9"/></root>"#
        );
        // 同一 rid 只收一次；r:link（外部链接）不收
        assert_eq!(blip_rids(xml), vec!["rId2".to_string(), "rId3".to_string()]);
        assert!(blip_rids("<root><a:t>x</a:t></root>").is_empty());
    }
}
