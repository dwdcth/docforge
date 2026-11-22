// 设计参考 firecrawl/anydoc 0.2.5（MIT，`tests/robustness.rs`），见 THIRD_PARTY_NOTICES.md。
//! 确定性字节变异鲁棒性测试：对每类已编译格式的合法样本做伪随机字节篡改与随机截断，
//! 断言 `docforge::parse` 绝不 panic、死循环或失控爆内存（允许返回类型化 `Err`）。

use std::path::PathBuf;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*：跨平台、跨运行确定性
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// 构造最小合法 ZIP 字节（STORED + CRC32，零外部依赖，任意 feature 组合可编）。
#[allow(dead_code)]
fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    let mut out = Vec::new();
    let mut cd = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let len = data.len() as u32;
        let nlen = name.len() as u16;
        // Local file header
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // STORED
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        // Central directory header
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&len.to_le_bytes());
        cd.extend_from_slice(&len.to_le_bytes());
        cd.extend_from_slice(&nlen.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&offset.to_le_bytes());
        cd.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    let cd_size = cd.len() as u32;
    let count = entries.len() as u16;
    out.extend_from_slice(&cd);
    // EOCD
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn synthetic_fixtures() -> Vec<(&'static str, Vec<u8>)> {
    #[allow(unused_mut)]
    let mut list: Vec<(&'static str, Vec<u8>)> = vec![
        (
            "sample.txt",
            "第一章 序言\n\n第一段正文。\n\n第二段正文。\n".as_bytes().to_vec(),
        ),
        (
            "sample.srt",
            b"1\n00:00:01,000 --> 00:00:02,500\nHello world\n\n2\n00:00:03,000 --> 00:00:04,000\nSecond\n"
                .to_vec(),
        ),
        (
            "sample.vtt",
            b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHi\n".to_vec(),
        ),
        (
            "sample.ass",
            b"[Script Info]\nTitle: T\n[Events]\nFormat: Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,Effect,Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Line 1\n"
                .to_vec(),
        ),
    ];

    #[cfg(feature = "md")]
    list.push((
        "sample.md",
        b"---\ntitle: T\n---\n# H1\n\n- a\n- b\n\n| k | v |\n|---|---|\n| 1 | 2 |\n\n```rs\nfn main(){}\n```\n"
            .to_vec(),
    ));

    #[cfg(feature = "csv")]
    list.push(("sample.csv", b"a,b,c\n\"1,2\",3,\"x\"\"y\"\n".to_vec()));

    #[cfg(feature = "html")]
    list.push((
        "sample.html",
        b"<html><body><h1>Title</h1><p>Para &amp; text</p><table><tr><th>A</th></tr><tr><td>1</td></tr></table></body></html>"
            .to_vec(),
    ));

    #[cfg(feature = "rtf")]
    list.push((
        "sample.rtf",
        br"{\rtf1\ansi{\fonttbl{\f0 Arial;}}Hello \u20013? world\par Second\par}".to_vec(),
    ));

    #[cfg(feature = "docx")]
    list.push((
        "sample.docx",
        build_stored_zip(&[(
            "word/document.xml",
            br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#,
        )]),
    ));

    #[cfg(feature = "xlsx")]
    list.push((
        "sample.xlsx",
        build_stored_zip(&[
            (
                "xl/workbook.xml",
                br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S1" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Hi</t></is></c></row></sheetData></worksheet>"#,
            ),
        ]),
    ));

    #[cfg(feature = "pptx")]
    list.push((
        "sample.pptx",
        build_stored_zip(&[
            (
                "ppt/presentation.xml",
                br#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst></p:presentation>"#,
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#,
            ),
            (
                "ppt/slides/slide1.xml",
                br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Slide 1</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
            ),
        ]),
    ));

    #[cfg(feature = "epub")]
    list.push((
        "sample.epub",
        build_stored_zip(&[
            ("mimetype", b"application/epub+zip"),
            (
                "META-INF/container.xml",
                br#"<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            ),
            (
                "OEBPS/content.opf",
                br#"<package xmlns="http://www.idpf.org/2007/opf" version="2.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Book</dc:title></metadata><manifest><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/></spine></package>"#,
            ),
            (
                "OEBPS/c1.xhtml",
                br#"<html xmlns="http://www.w3.org/1999/xhtml"><body><h1>Ch 1</h1><p>Body text</p></body></html>"#,
            ),
        ]),
    ));

    #[cfg(feature = "odt")]
    list.push((
        "sample.odt",
        build_stored_zip(&[
            ("mimetype", b"application/vnd.oasis.opendocument.text"),
            (
                "content.xml",
                br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:h text:outline-level="1">H</text:h><text:p>P</text:p></office:text></office:body></office:document-content>"#,
            ),
        ]),
    ));

    #[cfg(feature = "fb2")]
    list.push((
        "sample.fb2",
        br#"<?xml version="1.0" encoding="utf-8"?><FictionBook><description><title-info><book-title>T</book-title></title-info></description><body><section><title><p>S1</p></title><p>Hello</p></section></body></FictionBook>"#
            .to_vec(),
    ));

    #[cfg(feature = "archive")]
    list.push((
        "sample.zip",
        build_stored_zip(&[("ch1.txt", b"hello archive\n")]),
    ));

    list
}

#[test]
fn mutated_fixtures_never_panic() {
    let dir: PathBuf = std::env::temp_dir().join(format!("docforge_robust_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut rng = Rng(0x5EED_1234_5678_9ABC);

    for (name, original) in synthetic_fixtures() {
        let p = dir.join(name);
        // 原始合法样本首先应能不 panic 跑通
        std::fs::write(&p, &original).unwrap();
        let _ = docforge::parse(&p);

        for _ in 0..24 {
            let mut bytes = original.clone();
            let flips = 1 + (rng.next() % 8);
            for _ in 0..flips {
                let pos = (rng.next() as usize) % bytes.len();
                bytes[pos] = rng.next() as u8;
            }
            if rng.next().is_multiple_of(4) {
                let cut = (rng.next() as usize) % bytes.len();
                bytes.truncate(cut.max(1));
            }
            std::fs::write(&p, &bytes).unwrap();
            let _ = docforge::parse(&p);
        }
        let _ = std::fs::remove_file(&p);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
