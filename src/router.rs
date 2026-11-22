// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① crate 名引用替换；② head_of 改为 File::open 有界读取（消除整文件读入）；
// ③ 内容嗅探参考 firecrawl/anydoc 0.2.5（MIT，`src/formats/detect.rs`）：支持前导垃圾字节内的
// `%PDF-`、`{\rtf` magic，以及 `zip` 特性下的 ZIP 容器内部身份探测（mimetype / OPC rels /
// [Content_Types].xml / 主部件路径）。
//! 路由：按内容 magic + 容器身份 + 扩展名判定格式（与 Python `engine.Router` 语义对齐）。

use crate::error::{DocError, Result};
use std::io::Read;
use std::path::Path;

const OLE_MAGIC: &[u8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";
const PDF_MAGIC: &[u8] = b"%PDF";
const RTF_MAGIC: &[u8] = b"{\\rtf";
/// DjVu：IFF 容器，`AT&TFORM` + 4 字节文档类型（DJVM/DJVU），前 8 字节即够判
const DJVU_MAGIC: &[u8] = b"AT&TFORM";
/// 头部嗅探窗口（ISO 32000 允许 `%PDF-` 出现在前 1024 字节内）
const HEAD_SCAN_BYTES: usize = 1024;

const OOXML: &[&str] = &["docx", "docm", "xlsx", "xlsm", "xlsb", "pptx", "pptm", "ppsx", "ppsm"];
const ODF: &[&str] = &["odt", "ods", "odp"];
const OLE_EXT: &[&str] = &["doc", "ppt", "pps", "pot", "xls"];
const TEXT_EXT: &[&str] = &["txt", "md", "markdown", "csv", "html", "htm"];
/// 字幕：没有可靠 magic（.srt 首行是序号），按扩展名直连后端，内容合法性由后端判
const SUB_EXT: &[&str] = &["srt", "vtt", "ass", "ssa"];
/// RTF：有 magic（`{\rtf`），同时保留扩展名直连（伪装件由后端报 corrupt，不被当纯文本）
const RTF_EXT: &[&str] = &["rtf"];

/// 图像直读：按扩展名直连（内容合法性由 image-files 后端解码判），
/// 格式名 = 扩展名本身（jpg/jpeg、tif/tiff 各自成键，格式保真）
const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff"];

/// 有界读取文件前 `n` 字节（不再整文件读入内存）。
fn head_of(path: &Path, n: usize) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)
        .map_err(|e| DocError::corrupt(format!("cannot read {}: {e}", path.display())))?;
    let mut buf = vec![0u8; n];
    let mut read_total = 0usize;
    while read_total < n {
        match f.read(&mut buf[read_total..]) {
            Ok(0) => break,
            Ok(k) => read_total += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(DocError::corrupt(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        }
    }
    buf.truncate(read_total);
    Ok(buf)
}

/// 扩展名（小写）：取文件名末段 `.` 之后。无点时退化为整名（与原 `&str`
/// `rsplit('.')` 口径一致——整名不匹配任何已知扩展，路由结果不变）。
fn ext_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default()
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn ooxml_ext_format(ext: &str) -> Option<&'static str> {
    match ext {
        "docx" | "docm" => Some("docx"),
        "xlsx" | "xlsm" => Some("xlsx"),
        "xlsb" => Some("xlsb"),
        "pptx" | "pptm" | "ppsx" | "ppsm" => Some("pptx"),
        _ => None,
    }
}

fn ole_ext_format(ext: &str) -> Option<&'static str> {
    match ext {
        "doc" => Some("doc"),
        "ppt" | "pps" | "pot" => Some("ppt"),
        "xls" => Some("xls"),
        _ => None,
    }
}

/// 基于 ZIP 容器内部身份标记判定格式（mimetype → OPC `_rels/.rels` + `[Content_Types].xml`
/// → 常规主部件路径 → ODF `META-INF/manifest.xml` → EPUB `META-INF/container.xml`）。
/// 损坏/截断 ZIP 或普通压缩包返回 `None`，交由扩展名规则兜底。
#[cfg(any(
    feature = "docx",
    feature = "xlsx",
    feature = "pptx",
    feature = "xlsb",
    feature = "epub",
    feature = "fb2",
    feature = "archive",
    feature = "cbz",
    feature = "odt",
    feature = "ods",
    feature = "odp"
))]
mod zip_probe {
    use std::path::Path;

    pub(super) fn probe_zip_format(path: &Path) -> Option<&'static str> {
        let f = std::fs::File::open(path).ok()?;
        let mut zip = zip::ZipArchive::new(f).ok()?;
        if zip.len() > crate::limits::MAX_ENTRY_COUNT {
            return None;
        }

        let read_small = |z: &mut zip::ZipArchive<std::fs::File>, name: &str| -> Option<String> {
            let entry = z.by_name(name).ok()?;
            let bytes = crate::limits::read_bounded(entry, 512 * 1024, name).ok()?;
            String::from_utf8(bytes).ok()
        };

        // 1. ODF / EPUB 规范指定的 `mimetype` 条目
        if let Some(mime) = read_small(&mut zip, "mimetype") {
            if let Some(fmt) = mimetype_format(mime.trim()) {
                return Some(fmt);
            }
        }

        // 2. OPC `_rels/.rels` 的 officeDocument 关系 + `[Content_Types].xml`
        if let Some(rels) = read_small(&mut zip, "_rels/.rels") {
            if let Some(target) = find_office_document_target(&rels) {
                if let Some(ct_xml) = read_small(&mut zip, "[Content_Types].xml") {
                    if let Some(ct) = find_content_type(&ct_xml, &target) {
                        if let Some(fmt) = opc_content_type_format(&ct) {
                            return Some(fmt);
                        }
                    }
                }
                if let Some(fmt) = opc_path_format(&target) {
                    return Some(fmt);
                }
            }
        }

        // 3. 常规主部件存在性探测
        for (part, fmt) in [
            ("word/document.xml", "docx"),
            ("ppt/presentation.xml", "pptx"),
            ("xl/workbook.xml", "xlsx"),
            ("xl/workbook.bin", "xlsb"),
        ] {
            if zip.index_for_name(part).is_some() {
                return Some(fmt);
            }
        }

        // 4. 缺失 `mimetype` 的 ODF：回退 `META-INF/manifest.xml` 根条目 media-type
        if let Some(manifest) = read_small(&mut zip, "META-INF/manifest.xml") {
            if let Some(fmt) = manifest_root_format(&manifest) {
                return Some(fmt);
            }
        }

        // 5. 缺失 `mimetype` 的 EPUB：回退 `META-INF/container.xml`
        if zip.index_for_name("META-INF/container.xml").is_some() {
            return Some("epub");
        }

        None
    }

    fn mimetype_format(mime: &str) -> Option<&'static str> {
        let mime = mime.strip_suffix("-template").unwrap_or(mime);
        match mime {
            "application/epub+zip" => Some("epub"),
            "application/vnd.oasis.opendocument.text" => Some("odt"),
            "application/vnd.oasis.opendocument.spreadsheet" => Some("ods"),
            "application/vnd.oasis.opendocument.presentation" => Some("odp"),
            _ => None,
        }
    }

    fn opc_content_type_format(ct: &str) -> Option<&'static str> {
        let lower = ct.to_ascii_lowercase();
        if lower.contains("wordprocessingml") {
            Some("docx")
        } else if lower.contains("presentationml") {
            Some("pptx")
        } else if lower.contains("ms-excel") {
            Some("xlsb")
        } else if lower.contains("spreadsheetml") {
            Some("xlsx")
        } else {
            None
        }
    }

    fn opc_path_format(target: &str) -> Option<&'static str> {
        let clean = target.trim_start_matches('/').trim_start_matches("./");
        if clean.starts_with("word/") {
            Some("docx")
        } else if clean.starts_with("ppt/") {
            Some("pptx")
        } else if clean.starts_with("xl/") && clean.ends_with(".bin") {
            Some("xlsb")
        } else if clean.starts_with("xl/") {
            Some("xlsx")
        } else {
            None
        }
    }

    /// 轻量提取 XML 起始标签里的属性值（router 零 XML 依赖：仅用于小清单探测）。
    fn extract_xml_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        for quote in ['"', '\''] {
            let needle = format!("{name}={quote}");
            if let Some(pos) = tag.find(&needle) {
                let rest = &tag[pos + needle.len()..];
                if let Some(end) = rest.find(quote) {
                    return Some(&rest[..end]);
                }
            }
        }
        None
    }

    fn find_office_document_target(rels_xml: &str) -> Option<String> {
        for seg in rels_xml.split('<') {
            let tag = seg.split('>').next().unwrap_or("");
            if !tag.contains("Relationship") {
                continue;
            }
            let Some(rtype) = extract_xml_attr(tag, "Type") else {
                continue;
            };
            if rtype.ends_with("/officeDocument") {
                if let Some(target) = extract_xml_attr(tag, "Target") {
                    return Some(target.trim_start_matches('/').to_string());
                }
            }
        }
        None
    }

    fn find_content_type(ct_xml: &str, target: &str) -> Option<String> {
        let want = format!("/{}", target.trim_start_matches('/'));
        for seg in ct_xml.split('<') {
            let tag = seg.split('>').next().unwrap_or("");
            if !tag.contains("Override") {
                continue;
            }
            if let (Some(part), Some(ct)) = (
                extract_xml_attr(tag, "PartName"),
                extract_xml_attr(tag, "ContentType"),
            ) {
                if part.eq_ignore_ascii_case(&want) {
                    return Some(ct.to_string());
                }
            }
        }
        None
    }

    fn manifest_root_format(manifest_xml: &str) -> Option<&'static str> {
        for seg in manifest_xml.split('<') {
            let tag = seg.split('>').next().unwrap_or("");
            if !tag.contains("file-entry") {
                continue;
            }
            if extract_xml_attr(tag, "full-path") == Some("/") {
                if let Some(mime) = extract_xml_attr(tag, "media-type") {
                    return mimetype_format(mime.trim());
                }
            }
        }
        None
    }
}

/// 语义（继承 KB 现网经验）：
///   * OOXML 若文件头是 OLE 容器 → 加密件，归 encrypted；
///   * legacy（doc/ppt/xls）期望 OLE，不是 OLE → unsupported（伪装）；
///   * 容器 magic 优先于 `%PDF-` 窗口扫描（防止 ZIP 内嵌 PDF 误判）；
///   * 其余按头/magic 兜底。
///
/// `path` 吃任何 `AsRef<Path>`（&str / PathBuf 均可）。
pub fn sniff(path: impl AsRef<Path>) -> Result<&'static str> {
    let path = path.as_ref();
    let ext = ext_of(path);
    let h = head_of(path, HEAD_SCAN_BYTES)?;

    if h.starts_with(OLE_MAGIC) {
        if OOXML.contains(&ext.as_str()) {
            return Err(DocError::encrypted(format!("{ext} 为加密容器（OLE）")));
        }
        #[cfg(any(feature = "doc", feature = "ppt", feature = "xls"))]
        if let Some(res) = crate::backends::legacy_office::probe_ole_format(path) {
            return res;
        }
        if let Some(fmt) = ole_ext_format(&ext) {
            return Ok(fmt);
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
    if ext == "mobi" || ext == "prc" || ext == "azw3" {
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
        // fb2.zip：ZIP 容器内层单 .fb2（后端解包；内层校验由后端判）
        if ext == "zip"
            && path
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".fb2.zip")
        {
            return Ok("fb2");
        }
        // CBZ：ZIP + 图片条目（扩展名分流，条目白名单由后端校验）
        if ext == "cbz" {
            return Ok("cbz");
        }
        // 通用压缩包（.zip 扩展名）：内层文本多页合并
        if ext == "zip" {
            return Ok("zip");
        }
        // 有 zip 特性时先按容器内部身份探测（改名/错扩展名的 OOXML/ODF/EPUB 也能准确识别）；
        // 截断/无效 ZIP 回退扩展名分流，交由对应后端报 corrupt。
        #[cfg(any(
            feature = "docx",
            feature = "xlsx",
            feature = "pptx",
            feature = "xlsb",
            feature = "epub",
            feature = "fb2",
            feature = "archive",
            feature = "cbz",
            feature = "odt",
            feature = "ods",
            feature = "odp"
        ))]
        if let Some(fmt) = zip_probe::probe_zip_format(path) {
            return Ok(fmt);
        }
        // EPUB 也是 ZIP 容器：结构（container.xml/opf/spine）由 epub 后端进一步校验
        if ext == "epub" {
            return Ok("epub");
        }
        // ODF（ODT/ODS/ODP）同样是 ZIP 容器（内层 content.xml），按扩展名分流
        if ODF.contains(&ext.as_str()) {
            return Ok(match ext.as_str() {
                "odt" => "odt",
                "ods" => "ods",
                _ => "odp",
            });
        }
        // OOXML 扩展名回退
        if let Some(fmt) = ooxml_ext_format(&ext) {
            return Ok(fmt);
        }
        return Err(DocError::corrupt("ZIP 容器但缺少 OOXML 关键部件"));
    }
    if h.starts_with(RTF_MAGIC) {
        return Ok("rtf");
    }
    // PDF：前 1024 字节窗口内支持 `%PDF-`（ISO 32000 前导垃圾容忍），同时保留偏移 0 的 `%PDF`
    if h.starts_with(PDF_MAGIC) || h.windows(5).any(|w| w == b"%PDF-") {
        return Ok("pdf");
    }

    // CHM：ITSF 头 magic 直判；无 magic 的 .chm 扩展名兜底（伪装件由后端报 corrupt）
    if h.starts_with(b"ITSF") || ext == "chm" {
        return Ok("chm");
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
        assert_eq!(sniff(&pdf).unwrap(), "pdf");
        std::fs::remove_file(&pdf).ok();

        // magic 优先：.doc 但头是 %PDF → 按 pdf 路由（与 Python Router 一致）
        let renamed = d.join("renamed_pdf.doc");
        let mut f = std::fs::File::create(&renamed).unwrap();
        f.write_all(b"%PDF-1.7\n%%EOF").unwrap();
        drop(f);
        assert_eq!(sniff(&renamed).unwrap(), "pdf");
        std::fs::remove_file(&renamed).ok();

        // 伪装 doc：非 OLE / ZIP / PDF 头 → unsupported
        let fake = d.join("fake.doc");
        let mut f = std::fs::File::create(&fake).unwrap();
        f.write_all(b"NOTANOFFICEFILE").unwrap();
        drop(f);
        let err = sniff(&fake).unwrap_err();
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
        let err = sniff(&fx).unwrap_err();
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
            assert_eq!(sniff(&p).unwrap(), want, "{name}");
            std::fs::remove_file(&p).ok();
        }
    }

    /// RTF 虽有 `{\rtf` magic，仍按扩展名直连：头部合法性交给 rtf 后端判。
    #[test]
    fn rtf_extension_routes_to_rtf() {
        let d = std::env::temp_dir();
        let p = d.join("docparser_router_r.rtf");
        std::fs::write(&p, br"{\rtf1\ansi hi\par}").unwrap();
        assert_eq!(sniff(&p).unwrap(), "rtf");
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
            assert_eq!(sniff(&p).unwrap(), want, "{name}");
            std::fs::remove_file(&p).ok();
        }
    }

    #[test]
    fn pdf_leading_junk_and_rtf_magic() {
        let d = std::env::temp_dir();
        let p_pdf = d.join("docforge_router_leading_junk.bin");
        let mut junk = vec![b' '; 512];
        junk.extend_from_slice(b"%PDF-1.4\n%%EOF");
        std::fs::write(&p_pdf, &junk).unwrap();
        assert_eq!(sniff(&p_pdf).unwrap(), "pdf");
        std::fs::remove_file(&p_pdf).ok();

        let p_rtf = d.join("docforge_router_renamed_rtf.txt");
        std::fs::write(&p_rtf, br"{\rtf1\ansi hello\par}").unwrap();
        assert_eq!(sniff(&p_rtf).unwrap(), "rtf");
        std::fs::remove_file(&p_rtf).ok();
    }

    #[cfg(any(
        feature = "docx",
        feature = "xlsx",
        feature = "pptx",
        feature = "epub",
        feature = "fb2",
        feature = "archive",
        feature = "cbz",
        feature = "odt",
        feature = "ods",
        feature = "odp"
    ))]
    #[test]
    fn zip_content_detection_handles_renamed_containers_and_embedded_pdf() {
        let d = std::env::temp_dir();
        let make_zip = |fname: &str, parts: &[(&str, &[u8])]| {
            let p = d.join(fname);
            let f = std::fs::File::create(&p).unwrap();
            let mut w = zip::ZipWriter::new(f);
            for (name, bytes) in parts {
                w.start_file(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                w.write_all(bytes).unwrap();
            }
            w.finish().unwrap();
            p
        };

        // 改名 .bin 但内容是 docx，且首位嵌有 %PDF- 字节：容器身份必须胜出
        let p_docx = make_zip(
            "docforge_router_renamed_docx.bin",
            &[
                ("embedded.pdf", b"%PDF-1.7\n"),
                ("word/document.xml", b"<w:document/>"),
            ],
        );
        assert_eq!(sniff(&p_docx).unwrap(), "docx");
        std::fs::remove_file(&p_docx).ok();

        // 改名 .dat 但 mimetype 为 epub
        let p_epub = make_zip(
            "docforge_router_renamed_epub.dat",
            &[("mimetype", b"application/epub+zip")],
        );
        assert_eq!(sniff(&p_epub).unwrap(), "epub");
        std::fs::remove_file(&p_epub).ok();

        // OPC 非默认路径，靠 _rels/.rels + [Content_Types].xml 识别为 pptx
        let rels = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="/custom/main.xml"/>
        </Relationships>"#;
        let types = br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
            <Override PartName="/custom/main.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
        </Types>"#;
        let p_pptx = make_zip(
            "docforge_router_custom_pptx.bin",
            &[
                ("_rels/.rels", rels),
                ("[Content_Types].xml", types),
                ("custom/main.xml", b"<p/>"),
            ],
        );
        assert_eq!(sniff(&p_pptx).unwrap(), "pptx");
        std::fs::remove_file(&p_pptx).ok();
    }
}
