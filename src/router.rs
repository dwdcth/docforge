// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! 路由：按 magic + 扩展名判定格式（与 Python `engine.Router` 语义对齐）。

use crate::error::{DocError, Result};

const OLE_MAGIC: &[u8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";
const PDF_MAGIC: &[u8] = b"%PDF";
/// DjVu：IFF 容器，`AT&TFORM` + 4 字节文档类型（DJVM/DJVU），前 8 字节即够判
const DJVU_MAGIC: &[u8] = b"AT&TFORM";

const OOXML: &[&str] = &["docx", "xlsx", "pptx"];
const ODF: &[&str] = &["odt", "ods", "odp"];
const OLE_EXT: &[&str] = &["doc", "ppt", "xls"];
const TEXT_EXT: &[&str] = &["txt", "md", "markdown", "csv", "html", "htm"];
/// 字幕：没有可靠 magic（.srt 首行是序号），按扩展名直连后端，内容合法性由后端判
const SUB_EXT: &[&str] = &["srt", "vtt", "ass", "ssa"];
/// RTF：有 magic（`{\rtf`），但按扩展名直连，头部合法性由后端判（避免伪装件被当纯文本）
const RTF_EXT: &[&str] = &["rtf"];

/// 图像直读：按扩展名直连（内容合法性由 image-files 后端解码判），
/// 格式名 = 扩展名本身（jpg/jpeg、tif/tiff 各自成键，格式保真）
const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff"];

fn head_of(path: &str, n: usize) -> Result<Vec<u8>> {
    std::fs::read(path)
        .map(|v| v[..v.len().min(n)].to_vec())
        .map_err(|e| DocError::corrupt(format!("cannot read {path}: {e}")))
}

/// 语义（继承 KB 现网经验）：
///   * OOXML 若文件头是 OLE 容器 → 加密件，归 encrypted；
///   * legacy（doc/ppt/xls）期望 OLE，不是 OLE → unsupported（伪装）；
///   * 其余按头/magic 兜底。
pub fn sniff(path: &str) -> Result<&'static str> {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let h = head_of(path, 8)?;

    if h.starts_with(OLE_MAGIC) {
        if OOXML.contains(&ext.as_str()) {
            return Err(DocError::encrypted(format!("{ext} 为加密容器（OLE）")));
        }
        if OLE_EXT.contains(&ext.as_str()) {
            return Ok("ole");
        }
        return Err(DocError::unsupported(format!(
            "OLE 容器但扩展名不支持: {ext}"
        )));
    }
    if IMAGE_EXT.contains(&ext.as_str()) {
        // 返回 &'static（格式名 = 扩展名自身，逐键映射避免生命周期问题）
        return Ok(match ext.as_str() {
            "png" => "png",
            "jpg" => "jpg",
            "jpeg" => "jpeg",
            "webp" => "webp",
            "bmp" => "bmp",
            "tif" => "tif",
            _ => "tiff",
        });
    }

    if ext == "rar" {
        return Ok("rar");
    }
    if ext == "7z" {
        return Ok("7z");
    }

    // MOBI/PalmDOC：PalmDB 容器（BOOKMOBI magic 在 offset 60），按扩展名
    // 直连、头部校验由后端判（加密/DRM 在后端报 encrypted 终止码）
    if ext == "mobi" || ext == "prc" {
        return Ok("mobi");
    }

    // FB2：XML 文本无稳定 magic，按扩展名直连（根元素校验由后端判）
    if ext == "fb2" {
        return Ok("fb2");
    }

    if h.starts_with(DJVU_MAGIC) {
        return Ok("djvu");
    }
    if h.starts_with(ZIP_MAGIC) {
        // EPUB 也是 ZIP 容器：结构（container.xml/opf/spine）由 epub 后端进一步校验
        if ext == "epub" {
            return Ok("epub");
        }
        // CBZ：ZIP + 图片条目（扩展名分流，条目白名单由后端校验）
        if ext == "cbz" {
            return Ok("cbz");
        }
        // fb2.zip：ZIP 容器内层单 .fb2（后端解包；内层校验由后端判）
        if ext == "zip" && path.to_ascii_lowercase().ends_with(".fb2.zip") {
            return Ok("fb2");
        }
        // 通用压缩包（.zip 扩展名）：内层文本多页合并；.fb2.zip/.cbz 已在
        // 前面特判分流，此处通吃不受影响
        if ext == "zip" {
            return Ok("zip");
        }
        // ODF（ODT/ODS/ODP）同样是 ZIP 容器（内层 content.xml），按扩展名分流；
        // 真实类型由 docparser-odf 后端读 mimetype/content.xml 进一步校验
        if ODF.contains(&ext.as_str()) {
            return Ok(match ext.as_str() {
                "odt" => "odt",
                "ods" => "ods",
                _ => "odp",
            });
        }
        // 细判 ooxml 需要读包内清单，这里仅按扩展名近似（由解析后端进一步校验）
        if OOXML.contains(&ext.as_str()) {
            return Ok(match ext.as_str() {
                "docx" => "docx",
                "xlsx" => "xlsx",
                _ => "pptx",
            });
        }
        return Err(DocError::corrupt("ZIP 容器但缺少 OOXML 关键部件"));
    }
    if h.starts_with(PDF_MAGIC) {
        return Ok("pdf");
    }
    if RTF_EXT.contains(&ext.as_str()) {
        // 内容合法性（`{\rtf` 头）由 rtf 后端判；这里不抢判，避免伪装件被当纯文本
        return Ok("rtf");
    }
    if OLE_EXT.contains(&ext.as_str()) {
        return Err(DocError::unsupported(format!(
            "{ext} 文件头不是 OLE，疑似伪装"
        )));
    }
    if TEXT_EXT.contains(&ext.as_str()) {
        let kind: &'static str = match ext.as_str() {
            "txt" => "txt",
            "md" | "markdown" => "md",
            "csv" => "csv",
            "html" | "htm" => "html",
            _ => "txt",
        };
        return Ok(kind);
    }
    if SUB_EXT.contains(&ext.as_str()) {
        return Ok(subtitle_format(&ext));
    }
    Ok("txt") // 兜底当纯文本
}

fn subtitle_format(ext: &str) -> &'static str {
    match ext {
        "vtt" => "vtt",
        "ass" => "ass",
        "ssa" => "ssa",
        _ => "srt",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sniff_pdf_and_ole() {
        let d = std::env::temp_dir();
        let pdf = d.join("docparser_router_test.pdf");
        let mut f = std::fs::File::create(&pdf).unwrap();
        f.write_all(b"%PDF-1.7\n%%EOF").unwrap();
        drop(f);
        assert_eq!(sniff(pdf.to_str().unwrap()).unwrap(), "pdf");
        std::fs::remove_file(&pdf).ok();

        // magic 优先：.doc 但头是 %PDF → 按 pdf 路由（与 Python Router 一致）
        let renamed = d.join("renamed_pdf.doc");
        let mut f = std::fs::File::create(&renamed).unwrap();
        f.write_all(b"%PDF-1.7\n%%EOF").unwrap();
        drop(f);
        assert_eq!(sniff(renamed.to_str().unwrap()).unwrap(), "pdf");
        std::fs::remove_file(&renamed).ok();

        // 伪装 doc：非 OLE / ZIP / PDF 头 → unsupported
        let fake = d.join("fake.doc");
        let mut f = std::fs::File::create(&fake).unwrap();
        f.write_all(b"NOTANOFFICEFILE").unwrap();
        drop(f);
        let err = sniff(fake.to_str().unwrap()).unwrap_err();
        assert_eq!(err.code.as_str(), "unsupported");
        std::fs::remove_file(&fake).ok();
    }

    #[test]
    fn ooxml_ole_container_is_encrypted() {
        let d = std::env::temp_dir();
        let fx = d.join("enc.docx");
        let mut f = std::fs::File::create(&fx).unwrap();
        f.write_all(OLE_MAGIC).unwrap();
        drop(f);
        let err = sniff(fx.to_str().unwrap()).unwrap_err();
        assert_eq!(err.code.as_str(), "encrypted");
        std::fs::remove_file(&fx).ok();
    }

    /// 字幕没有可靠 magic：按扩展名直连（内容合法性由 subtitle 后端判）。
    #[test]
    fn subtitle_extensions_route_to_subtitle_formats() {
        let d = std::env::temp_dir();
        for (name, want) in [
            ("r.srt", "srt"),
            ("r.vtt", "vtt"),
            ("r.ass", "ass"),
            ("r.ssa", "ssa"),
        ] {
            let p = d.join(format!("docparser_router_{name}"));
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(b"1\n00:00:01,000 --> 00:00:02,000\nhi\n")
                .unwrap();
            drop(f);
            assert_eq!(sniff(p.to_str().unwrap()).unwrap(), want, "{name}");
            std::fs::remove_file(&p).ok();
        }
    }

    /// RTF 虽有 `{\rtf` magic，仍按扩展名直连：头部合法性交给 rtf 后端判。
    #[test]
    fn rtf_extension_routes_to_rtf() {
        let d = std::env::temp_dir();
        let p = d.join("docparser_router_r.rtf");
        std::fs::write(&p, br"{\rtf1\ansi hi\par}").unwrap();
        assert_eq!(sniff(p.to_str().unwrap()).unwrap(), "rtf");
        std::fs::remove_file(&p).ok();
    }

    /// ODF 也是 ZIP 容器：按扩展名分流到 odt / ods / odp（内容由 odf 后端再校验）。
    #[test]
    fn zip_container_routes_odf_family() {
        let d = std::env::temp_dir();
        for (name, want) in [
            ("r.odt", "odt"),
            ("r.ods", "ods"),
            ("r.odp", "odp"),
            ("r.docx", "docx"),
        ] {
            let p = d.join(format!("docparser_router_{name}"));
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(ZIP_MAGIC).unwrap();
            drop(f);
            assert_eq!(sniff(p.to_str().unwrap()).unwrap(), want, "{name}");
            std::fs::remove_file(&p).ok();
        }
    }
}
