// docforge 新增（翻译闭环①写侧）：ODT 双语回写。
//! ODT 双语回写：ZIP 透传 + `content.xml` 段落级定位改写（feature = "odt"）。
//!
//! # 对齐口径（与 `backends/odf/odt.rs` + `backends/odf/mod.rs` 提取侧逐条对应）
//!
//! * 提取侧管线：`OdtRawBackend`（`content.xml` 的 `office:text` 下逐段：
//!   `text:h`（`text:outline-level`，缺失按 1）→ 标题 item、`text:p` → 段落
//!   item，列表/小节等容器递归、内部段落照收，`table:table` 整棵进
//!   `tables`）→ `OdfPlugin::assemble`（item 块在前、**表格块统一排在段后**）。
//!   ODT 单页（无物理页），id = `p0-b{块序}`。回写侧**重跑同一管线**拿权威
//!   id/文本，再以同一套遍历（`odt_raw_page` 的镜像）**重扫描 content.xml**，
//!   把每个 item 映回「槽位 × 字节锚点」。
//! * 定位锚点：`text:h` / `text:p` 元素——Inline 在元素 End 后插
//!   `<text:p text:style-name="原样式">` 译段（样式沿用原段，见下）；
//!   TranslationOnly 替换元素**内部文本区**（`text:s` / `text:tab` /
//!   `text:line-break` 与内联标记一并被译文替换——提取侧文本本就按
//!   `text_until` 拍平，重解析口径等价）；表格单元格内的 `text:p` 不进
//!   翻译流也不占槽位（提取侧表格只产 Table 块）。
//! * 样式沿用：`text:style-name` 属性**原样复制**（源属性值字节透传，
//!   不重编码）——译段与原段同款段落样式，阅读器渲染一致。
//! * 双保险：每个槽位文本与权威单元文本一致性比对（`text_until` 同口径：
//!   `text:s`/`text:tab`/`text:line-break` 转换、纯空白文本节点丢弃、实体
//!   解码），不一致跳过该块（宁漏勿错）；未匹配 id / 空白译文一律忽略。
//! * OriginalPlusAppendix：正文不动，`</office:text>` 前追加
//!   `<text:h text:outline-level="1">译文</text:h>` + 译文段序列。
//! * ZIP：其余条目字节透传（`mimetype` 首条目 + Stored 约定由共用
//!   `rewrite_zip` 维护，ODT 与 EPUB 同规）。

use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::backends::odf::{attr, push_general_ref, repeat_of, tags, zip_read_str, OdfPlugin};
use crate::backends::odf::odt::OdtRawBackend;
use crate::emit::BiStrategy;
use crate::error::{DocError, Result};
use crate::plugin::{FormatPlugin, ParseCapability};

use super::{apply_edits, Edit, escape_xml_text, open_zip, rewrite_zip, translation_map};

/// odt 双语回写：见模块级文档。`translated` 的 id 与
/// [`crate::emit::to_translation_units`] 同口径（`p0-b{块序}`）；未匹配 /
/// 空白译文条目忽略；失败返回 `corrupt`。
pub fn writeback_odt(
    in_path: &Path,
    translated: &[(String, String)],
    strategy: BiStrategy,
    out: &Path,
) -> Result<()> {
    let path_str = in_path
        .to_str()
        .ok_or_else(|| DocError::corrupt("odt 路径不是 UTF-8"))?;

    // A. 权威解析（提取侧同管线）拿 id / 文本
    let raw = OdtRawBackend.parse(path_str)?;
    let doc = OdfPlugin.assemble(&raw);
    let units: Vec<_> = crate::emit::to_translation_units(&doc)
        .into_iter()
        // 表格单元格单元（kind = Table）不进本回写：内联候选复刻（is_translatable
        // 本就排除 Table，表格块装配时统一排在段后）按块序消费 units[cursor]，
        // 单元流同步排除才不错位；表格在 content.xml 里保持原样。
        // TODO(表格回写 wave2)：单元格级替换（table:table 树扫描 + column span 对齐）
        .filter(|u| u.kind != crate::model::BlockKind::Table)
        .collect();
    let map = translation_map(translated);
    if doc.pages.len() != 1 {
        return Err(DocError::corrupt("odt 对齐失败：Document 页数异常"));
    }

    // B. content.xml 读出（与解析侧同一条 UTF-8 门禁）+ 槽位重扫描
    let mut zip = open_zip(in_path)?;
    let xml = zip_read_str(&mut zip, "content.xml")?
        .ok_or_else(|| DocError::corrupt("missing content.xml（不是 ODT 包）"))?;
    let scanned = scan_odt(&xml)?;
    let page = &doc.pages[0];
    if scanned.slots.len() > page.blocks.len() {
        return Err(DocError::corrupt(
            "odt 对齐失败：重扫描槽位数多于解析块数",
        ));
    }

    // C. 块 × 单元按序配对（同一过滤口径）+ 一致性守卫，逐策略产编辑
    let mut edits: Vec<Edit> = Vec::new();
    let mut matched: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    for (b_idx, b) in page.blocks.iter().enumerate() {
        let t = b.content.trim();
        if !crate::emit::is_translatable(b.kind) || t.is_empty() || t.chars().count() < 2 {
            continue; // 表格块（装配时统一排在段后）在此自然跳过
        }
        if b_idx >= scanned.slots.len() || cursor >= units.len() {
            break;
        }
        let unit = &units[cursor];
        cursor += 1;
        let slot = &scanned.slots[b_idx];
        // 双保险：重扫描文本必须与权威单元文本一致
        if slot.text != unit.text {
            continue;
        }
        let Some(tr) = map.get(unit.id.as_str()) else {
            continue;
        };
        let tr = tr.trim();
        if tr.is_empty() {
            continue;
        }
        matched.push(tr.to_string());
        match strategy {
            BiStrategy::BilingualInline => {
                let style = slot
                    .style
                    .as_deref()
                    .map(|s| format!(" text:style-name=\"{s}\""))
                    .unwrap_or_default();
                edits.push(Edit::Insert(
                    slot.after,
                    format!("<text:p{style}>{}</text:p>", escape_xml_text(tr)),
                ));
            }
            BiStrategy::TranslationOnly => {
                let (s, e) = slot.inner;
                if s < e {
                    edits.push(Edit::Replace(s, e, escape_xml_text(tr)));
                }
            }
            BiStrategy::OriginalPlusAppendix => {}
        }
    }

    // Appendix：正文不动，</office:text> 前追加「译文」标题 + 译文段
    if strategy == BiStrategy::OriginalPlusAppendix && !matched.is_empty() {
        let mut appendix = String::from("<text:h text:outline-level=\"1\">译文</text:h>");
        for tr in &matched {
            appendix.push_str(&format!("<text:p>{}</text:p>", escape_xml_text(tr)));
        }
        edits.push(Edit::Insert(scanned.text_close, appendix));
    }

    // D. 改写并重打包（其余条目透传；无命中 = content.xml 原样）
    let new_xml = if edits.is_empty() {
        xml
    } else {
        apply_edits(&xml, edits)
    };
    rewrite_zip(
        in_path,
        out,
        &[("content.xml".to_string(), new_xml.into_bytes())],
    )
}

// ---------------------------------------------------------------------------
// content.xml 槽位重扫描（odt_raw_page 的镜像，带字节锚点）
// ---------------------------------------------------------------------------

/// 一个 item 的重扫描定位。
struct OdtSlot {
    /// 与提取侧 `text_until` 同口径的元素文本（trim 后）。
    text: String,
    /// 原元素 `text:style-name` 属性值（原样字节；无则 None）。
    style: Option<String>,
    /// 元素 End 之后的字节偏移（BilingualInline 插位）。
    after: usize,
    /// 元素内部文本区 `[inner_start, inner_end)`（TranslationOnly 替换区）。
    inner: (usize, usize),
}

/// 重扫描结果：槽位序列（item 序）+ `</office:text>` 起始偏移。
struct Scanned {
    slots: Vec<OdtSlot>,
    text_close: usize,
}

/// quick-xml 0.42 的 `buffer_position()` 是 `u64`；本模块 span 统一 `usize`。
fn bpos<R: std::io::BufRead>(reader: &Reader<R>) -> usize {
    reader.buffer_position() as usize
}

/// 扫描 `office:text`：与 [`crate::backends::odf::odt::odt_raw_page`] 完全
/// 同款的遍历（h/p 逐元素消费、table 整棵跳过、其余容器深度递归），附带
/// 字节锚点与 `</office:text>` 位置。
fn scan_odt(xml: &str) -> Result<Scanned> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();

    // 定位 <office:text>（解析侧同款前置循环）
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().local_name().as_ref() == tags::TEXT => break,
            Ok(Event::Eof) => return Err(DocError::corrupt("no office:text found（不是 ODT）")),
            Err(e) => return Err(DocError::corrupt(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    buf.clear();

    let mut slots: Vec<OdtSlot> = Vec::new();
    let mut text_close = xml.len();
    let mut depth = 1usize;
    loop {
        let pos_before = bpos(&reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().local_name();
                match name.as_ref() {
                    tags::H | tags::P => {
                        let style = attr(e, "style-name");
                        let (text, inner, after) = consume_text_elem(&mut reader, name.as_ref())?;
                        // 解析侧 push_item 同口径：trim 非空才产 item
                        let t = text.trim();
                        if !t.is_empty() {
                            slots.push(OdtSlot {
                                text: t.to_string(),
                                style,
                                after,
                                inner,
                            });
                        }
                    }
                    tags::TABLE => skip_subtree(&mut reader)?,
                    // list / section / tracked-changes 等：递归进去，内部的
                    // text:h / text:p 照常收集（解析侧同口径）
                    _ => depth += 1,
                }
            }
            Ok(Event::End(ref _e)) => {
                depth -= 1;
                if depth == 0 {
                    text_close = pos_before; // `</office:text>` 的起始偏移
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(Scanned { slots, text_close })
}

/// 跳过当前子树直到配对 End（表格等非候选元素；解析侧 table_rows 同款语义）。
fn skip_subtree<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<()> {
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref _e)) => depth += 1,
            Ok(Event::End(ref _e)) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            Ok(Event::Eof) => return Ok(()),
            Err(e) => return Err(DocError::corrupt(format!("XML subtree scan error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
}

/// 消费一个 `text:h` / `text:p` 元素：文本与提取侧
/// [`crate::backends::odf::text_until`] 完全同口径（`text:s` / `text:tab` /
/// `text:line-break` 转换、其余标签透明、纯空白文本节点丢弃、实体解码），
/// 附带内部文本区与 End 后偏移。
fn consume_text_elem<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    end: &str,
) -> Result<(String, (usize, usize), usize)> {
    use quick_xml::events::Event as E;

    let inner_start = bpos(reader);
    let mut out = String::new();
    let mut buf = Vec::new();
    let mut depth = 1usize;
    let (mut inner_end, mut after) = (inner_start, inner_start);
    loop {
        let pos_before = bpos(reader);
        match reader.read_event_into(&mut buf) {
            Ok(E::Start(ref e)) => match e.name().local_name().as_ref() {
                tags::S => out.push_str(&" ".repeat(repeat_of(e, tags::C))),
                tags::TAB => out.push('\t'),
                tags::LINE_BREAK => out.push('\n'),
                _ => depth += 1,
            },
            Ok(E::Empty(ref e)) => match e.name().local_name().as_ref() {
                tags::S => out.push_str(&" ".repeat(repeat_of(e, tags::C))),
                tags::TAB => out.push('\t'),
                tags::LINE_BREAK => out.push('\n'),
                _ => {}
            },
            Ok(E::End(ref e)) => {
                if e.name().local_name().as_ref() == end {
                    depth -= 1;
                    if depth == 0 {
                        inner_end = pos_before;
                        after = bpos(reader);
                        break;
                    }
                } else {
                    depth = depth.saturating_sub(1);
                }
            }
            Ok(E::Text(ref e)) => {
                // 纯缩进空白节点丢弃（ODF 的显式空格走 text:s；解析侧同口径）
                let raw = e.xml10_content();
                if !raw.trim().is_empty() {
                    if let Ok(t) = quick_xml::escape::unescape(&raw) {
                        out.push_str(&t);
                    }
                }
            }
            Ok(E::GeneralRef(ref e)) => push_general_ref(&mut out, e),
            Ok(E::CData(ref e)) => out.push_str(e.as_ref()),
            Ok(E::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok((out, (inner_start, inner_end), after))
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::path::PathBuf;

    use crate::emit::to_translation_units;
    use crate::model::BlockKind::{Heading, Paragraph, Table};

    const HEAD: &str = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:text>"#;
    const TAIL: &str = "</office:text></office:body></office:document-content>";

    /// 正文 fixture：h + 样式段 + 短段（1 字符）+ 列表内段 + 表格 + 尾段。
    const BODY: &str = concat!(
        r#"<text:h text:outline-level="1" text:style-name="Heading_20_1">标题一</text:h>"#,
        r#"<text:p text:style-name="Standard">正文一段。</text:p>"#,
        r#"<text:p>乙</text:p>"#,
        r#"<text:list><text:list-item><text:p>列表项甲。</text:p></text:list-item></text:list>"#,
        r#"<table:table table:name="T"><table:table-row>"#,
        r#"<table:table-cell><text:p>格甲</text:p></table:table-cell>"#,
        r#"</table:table-row></table:table>"#,
        r#"<text:p>尾段一句。</text:p>"#,
    );

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_odt_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_odt(path: &Path, body: &str) {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        zw.start_file(
            "mimetype",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zw.write_all(b"application/vnd.oasis.opendocument.text").unwrap();
        zw.start_file("styles.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(b"<office:document-styles/>").unwrap();
        zw.start_file("content.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(format!("{HEAD}{body}{TAIL}").as_bytes())
            .unwrap();
        zw.finish().unwrap();
    }

    /// 提取侧同管线（OdtRawBackend + OdfPlugin）。
    fn parse_doc(path: &Path) -> crate::model::Document {
        let raw = OdtRawBackend.parse(path.to_str().unwrap()).unwrap();
        OdfPlugin.assemble(&raw)
    }

    /// 回写消费的单元 id（writeback_odt 过滤 Table 单元格单元，wave2 前
    /// 表格不回写）——helper 与该消费口径一致，不是提取侧全量单元流。
    fn unit_ids(path: &Path) -> Vec<String> {
        to_translation_units(&parse_doc(path))
            .into_iter()
            .filter(|u| u.kind != Table)
            .map(|u| u.id)
            .collect()
    }

    fn write(in_path: &Path, out_path: &Path, tr: &[(&str, &str)], strategy: BiStrategy) {
        let translated: Vec<(String, String)> =
            tr.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        writeback_odt(in_path, &translated, strategy, out_path).unwrap();
    }

    fn entry_str(zip_path: &Path, name: &str) -> String {
        let mut z = open_zip(zip_path).unwrap();
        let mut s = String::new();
        z.by_name(name).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    /// 金测（核心验收）：BilingualInline 往返 —— 标题/段落/列表段原/译交错
    /// （译段沿用原段样式）、表格单元格不受扰、1 字符短段不占翻译位、
    /// 未匹配 id 忽略、其余条目透传、mimetype 仍居首。
    #[test]
    fn roundtrip_bilingual_inline() {
        let dir = case_dir("inline");
        let in_path = dir.join("in.odt");
        build_odt(&in_path, BODY);
        let out_path = dir.join("out.odt");

        // 回写消费 id 口径（乙 1 字符不发锚点；表格块排段后占 b5，其格单元
        // 进提取侧流但被回写器过滤，wave2 起可回写）
        assert_eq!(unit_ids(&in_path), ["p0-b0", "p0-b1", "p0-b3", "p0-b4"]);

        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "Heading 1"),
                ("p0-b1", "First paragraph."),
                ("p0-b3", "List item A."),
                ("p0-b4", "Final sentence."),
                ("p0-b99", "bogus"), // 未匹配 id：忽略
            ],
            BiStrategy::BilingualInline,
        );

        // 重解析：原/译交错；表格不受扰
        let blocks = &parse_doc(&out_path).pages[0].blocks;
        let kinds: Vec<crate::model::BlockKind> = blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                Heading, Paragraph, // 标题一 + 译段
                Paragraph, Paragraph, // 正文一段。 + 译段
                Paragraph, // 乙：不占翻译位，原样
                Paragraph, Paragraph, // 列表项甲。 + 译段
                Paragraph, Paragraph, // 尾段一句。 + 译段
                Table,     // 表格不受扰
            ]
        );
        let contents: Vec<&str> = blocks.iter().map(|b| b.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "标题一", "Heading 1",
                "正文一段。", "First paragraph.",
                "乙",
                "列表项甲。", "List item A.",
                "尾段一句。", "Final sentence.",
                "格甲",
            ]
        );
        assert_eq!(blocks[9].table.as_ref().unwrap().rows, vec![vec!["格甲".to_string()]]);

        // 译段沿用原段 text:style-name；未匹配 id 不落盘
        let content_xml = entry_str(&out_path, "content.xml");
        assert!(content_xml.contains(r#"<text:p text:style-name="Heading_20_1">Heading 1</text:p>"#));
        assert!(content_xml.contains(r#"<text:p text:style-name="Standard">First paragraph.</text:p>"#));
        assert!(!content_xml.contains("bogus"));

        // ZIP 层：条目数不变、mimetype 居首且 Stored、styles.xml 字节透传
        let mut zin = open_zip(&in_path).unwrap();
        let mut zout = open_zip(&out_path).unwrap();
        assert_eq!(zout.len(), zin.len());
        assert_eq!(zout.by_index(0).unwrap().name().unwrap(), "mimetype");
        assert_eq!(
            zout.by_index(0).unwrap().compression(),
            zip::CompressionMethod::Stored
        );
        let mut a = String::new();
        let mut b = String::new();
        zin.by_name("styles.xml").unwrap().read_to_string(&mut a).unwrap();
        zout.by_name("styles.xml").unwrap().read_to_string(&mut b).unwrap();
        assert_eq!(a, b);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TranslationOnly：元素内文本区替换（含列表内段与标题）；表格单元格与
    /// 未翻译段原样；无插入。
    #[test]
    fn translation_only_replaces_text() {
        let dir = case_dir("tonly");
        // 实体 + 显式空格考点：&amp; 与 text:s 在提取侧文本里已拍平
        let body = concat!(
            r#"<text:h text:outline-level="1">标题一</text:h>"#,
            r#"<text:p>含 &amp; 实体与<text:s text:c="2"/>空格的段落。</text:p>"#,
            r#"<text:p>保留原文。</text:p>"#,
        );
        let in_path = dir.join("in.odt");
        build_odt(&in_path, body);
        let out_path = dir.join("out.odt");
        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "Heading 1"),
                ("p0-b1", "Entity & spaces replaced."),
            ],
            BiStrategy::TranslationOnly,
        );

        let blocks = &parse_doc(&out_path).pages[0].blocks;
        assert_eq!(blocks.len(), 3, "无插入：块数与原文一致");
        assert_eq!(blocks[0].kind, Heading);
        assert_eq!(blocks[0].content, "Heading 1");
        assert_eq!(blocks[1].content, "Entity & spaces replaced.", "元素内文本区整体替换");
        assert_eq!(blocks[2].content, "保留原文。", "未翻译单元原样");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：正文不动，</office:text> 前追加「译文」标题 +
    /// 译文段；全部未匹配时 content.xml 逐字节透传。
    #[test]
    fn appendix_appends_before_text_close() {
        let dir = case_dir("appendix");
        let in_path = dir.join("in.odt");
        build_odt(&in_path, BODY);
        let out_path = dir.join("out.odt");
        write(
            &in_path,
            &out_path,
            &[("p0-b1", "First paragraph."), ("p0-b4", "Final sentence.")],
            BiStrategy::OriginalPlusAppendix,
        );

        let content_xml = entry_str(&out_path, "content.xml");
        let close = content_xml.rfind("</office:text>").unwrap();
        let h = content_xml.find(r#"<text:h text:outline-level="1">译文</text:h>"#).unwrap();
        assert!(h < close, "附录在 </office:text> 之前");
        assert!(content_xml.contains("正文一段。"), "正文不动");

        // 重解析：item 块在前（附录标题 + 译段随 XML 序落在最后），表格块
        // 由 assemble 统一排到全部段后
        let blocks = &parse_doc(&out_path).pages[0].blocks;
        let contents: Vec<&str> = blocks.iter().map(|b| b.content.as_str()).collect();
        let tail: Vec<&str> = contents[contents.len() - 4..].to_vec();
        assert_eq!(tail, ["译文", "First paragraph.", "Final sentence.", "格甲"]);
        assert_eq!(blocks[blocks.len() - 4].kind, Heading);

        // 全部未匹配：content.xml 逐字节透传
        let out2 = dir.join("out2.odt");
        write(&in_path, &out2, &[("p0-b42", "nope")], BiStrategy::BilingualInline);
        assert_eq!(entry_str(&out2, "content.xml"), entry_str(&in_path, "content.xml"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
