// docforge 新增（翻译闭环①写侧）：docx 双语回写。
//! DOCX 双语回写：解包原 ZIP → 重解析建立块序对齐 → 改写 `word/document.xml`
//! → 其余部件字节透传，重打包到 `out`。
//!
//! # 对齐口径（与 `backends/office/docx.rs` 解析侧逐条对应）
//!
//! * 解析侧 body 直属子级 `w:p`（trim 非空）→ 文本项、`w:tbl` → 表格项、
//!   `w:sdt` → 内部段落项；`assemble` 按 item 序产块（表格 > 标题 > 列表 > 段落）。
//!   因此「第 k 个可翻译块」↔「XML 扫描的第 k 个合格段落位」：
//!   合格 = trim 非空 + 字符数 ≥ 2（[`crate::emit::to_translation_units`] 同口径）；
//!   **表格内的 `w:p` 不占翻译位**（解析侧表格只产一个 Table 块，不进翻译流），
//!   本扫描遇 `w:tbl` 整棵子树跳过；自闭合 `<w:p/>` 无文本，同样不占位。
//! * 文本提取语义与解析侧 `extract_para` 相同：只收 `w:t` 文本（含实体解码
//!   / CDATA），`w:tab`→`\t`、`w:br`/`w:cr`→`\n`、`w:noBreakHyphen`→`-`；
//!   文本框等嵌套 `w:p` 的 `w:t` 并入外层段落文本（解析侧同此行为）。
//! * 双保险：每个段落位与重解析的对应块做文本一致性比对，不一致跳过该块
//!   （防解析器演进后错位写）。
//! * `w:sdt`（内容控件，正文里多为 TOC）：取其子树内的 `w:p` 作候选位。
//!   已知差异：解析侧 `extract_sdt_items` 的深度计数在提取段落后不回落，
//!   sdt 之后的表格会被吞进 sdt 循环（上游特性）；本扫描按良构深度跟踪，
//!   sdt 之后的表格照常跳过、段落照常成位——若文档真踩到此差异，
//!   文本一致性守卫会让后续段落保守跳过而不是错位写。

use std::collections::HashMap;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::backends::office::docx::DocxRawBackend;
use crate::backends::office::{tags, OfficePlugin};
use crate::emit::BiStrategy;
use crate::error::{DocError, Result};
use crate::plugin::{FormatPlugin, ParseCapability};

use super::{escape_xml_text, open_zip, rewrite_zip, translation_map};

/// docx 双语回写：见模块级文档。`translated` 的 id 与
/// [`crate::emit::to_translation_units`] 同口径（`p{页}-b{块序}`）；
/// 未匹配 / 空白译文条目忽略；失败返回 `corrupt`。
pub fn writeback_docx(
    in_path: &Path,
    translated: &[(String, String)],
    strategy: BiStrategy,
    out: &Path,
) -> Result<()> {
    let path_str = in_path
        .to_str()
        .ok_or_else(|| DocError::corrupt("docx 路径不是 UTF-8"))?;

    // 第一遍：docforge 解析（与提取侧 register_defaults 完全相同的默认装配：
    // structure_mode=auto / include_regions=false / ocr_images=false）拿块序与 id。
    let raw = DocxRawBackend::default().parse(path_str)?;
    let doc = OfficePlugin::default().assemble(&raw);
    let units = crate::emit::to_translation_units(&doc);
    let map = translation_map(translated);

    // 第二遍：XML 扫描段落位，与单元按序对齐。
    let mut zip = open_zip(in_path)?;
    let xml = crate::backends::office::zip_read_str(&mut zip, "word/document.xml")?
        .ok_or_else(|| DocError::corrupt("missing word/document.xml"))?;
    let (slots, body_close) = scan_para_slots(&xml)?;

    let new_xml = match strategy {
        BiStrategy::BilingualInline => apply_inline(&xml, &slots, &units, &map),
        BiStrategy::TranslationOnly => apply_translation_only(&xml, &slots, &units, &map),
        BiStrategy::OriginalPlusAppendix => apply_appendix(&xml, &slots, &units, &map, body_close),
    };

    rewrite_zip(
        in_path,
        out,
        &[("word/document.xml".to_string(), new_xml.into_bytes())],
    )
}

// ---------------------------------------------------------------------------
// 段落位扫描（quick-xml 流式，带字节 span）
// ---------------------------------------------------------------------------

/// 一个「可翻译段落位」：`<w:p>` 的字节 span + 与解析侧同口径的文本 + 原段 pPr。
#[derive(Debug, Clone)]
struct ParaSlot {
    /// `</w:p>` 结束字节偏移（不含其后内容；BilingualInline 插位）。
    end: usize,
    /// 与解析侧 `extract_para` 同口径的段落文本（未 trim；判定用 trim）。
    text: String,
    /// 原段 `<w:pPr ...>...</w:pPr>` 原文（无则 None）——BilingualInline 复制样式用。
    ppr: Option<String>,
    /// 段内 `w:t` 文本内容的字节 span（Text/GeneralRef/CData 事件级），
    /// TranslationOnly 用：首段换译文、其余清空。
    text_spans: Vec<(usize, usize)>,
}

impl ParaSlot {
    /// 合格翻译位判定：与 `to_translation_units` 同口径（trim 非空 + ≥2 字符）。
    fn is_candidate(&self) -> bool {
        let t = self.text.trim();
        !t.is_empty() && t.chars().count() >= 2
    }
}

/// quick-xml 0.42 的 `buffer_position()` 是 `u64`；本模块 span 统一 `usize`。
fn bpos<R: std::io::BufRead>(reader: &Reader<R>) -> usize {
    reader.buffer_position() as usize
}

/// 扫描 body 直属子级，产出文档序的合格段落位与 `</w:body>` 起始偏移。
/// `w:tbl` 整棵跳过（表格不进翻译流）；`w:sdt` 取子树内段落。
fn scan_para_slots(xml: &str) -> Result<(Vec<ParaSlot>, usize)> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    // 定位 <w:body>（解析侧同款前置循环）
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().local_name().as_ref() == tags::W_BODY => break,
            Ok(Event::Eof) => return Err(DocError::corrupt("no w:body found")),
            Err(e) => return Err(DocError::corrupt(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    buf.clear();

    let mut slots: Vec<ParaSlot> = Vec::new();
    let body_close = loop {
        let pos_before = bpos(&reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name().local_name().as_ref() {
                tags::W_P => {
                    let slot = consume_para(&mut reader, xml)?;
                    if slot.is_candidate() {
                        slots.push(slot);
                    }
                }
                tags::W_TBL => consume_subtree(&mut reader)?,
                tags::W_SDT => consume_sdt(&mut reader, xml, &mut slots)?,
                _ => {}
            },
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == tags::W_BODY => {
                break pos_before
            }
            Ok(Event::Eof) => break xml.len(),
            Err(e) => return Err(DocError::corrupt(format!("XML body scan error: {e}"))),
            _ => {}
        }
        buf.clear();
    };
    Ok((slots, body_close))
}

/// 消费一个 `<w:p>` 子树（调用时 reader 刚读完 `<w:p>` Start），返回段落位。
/// 文本/占位语义与解析侧 `extract_para` 一致。
fn consume_para<R: std::io::BufRead>(reader: &mut Reader<R>, xml: &str) -> Result<ParaSlot> {
    let mut buf = Vec::new();
    let mut text = String::new();
    let mut text_spans: Vec<(usize, usize)> = Vec::new();
    let mut ppr: Option<String> = None;
    let mut ppr_start = 0usize;
    let mut in_t = false;
    let mut depth = 1usize;

    loop {
        let pos_before = bpos(reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match e.name().local_name().as_ref() {
                    // pPr 在跑文本之前：整棵子树按普通元素消费，只留原文 span
                    tags::W_PPR => {
                        ppr_start = pos_before;
                        depth += 1;
                    }
                    tags::W_T => {
                        in_t = true;
                        depth += 1;
                    }
                    tags::W_TAB => {
                        depth += 1;
                        text.push('\t');
                    }
                    tags::W_BR | tags::W_CR => {
                        depth += 1;
                        text.push('\n');
                    }
                    tags::W_NBH => {
                        depth += 1;
                        text.push('-');
                    }
                    _ => depth += 1,
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name().local_name().as_ref().to_string();
                if name == tags::W_T {
                    in_t = false;
                }
                if name == tags::W_PPR {
                    // 事件读完后的 position = `</w:pPr>` 之后 → 原 pPr 全文
                    ppr = Some(xml[ppr_start..bpos(reader)].to_string());
                }
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Empty(ref e)) => match e.name().local_name().as_ref() {
                tags::W_TAB => text.push('\t'),
                tags::W_BR | tags::W_CR => text.push('\n'),
                tags::W_NBH => text.push('-'),
                _ => {}
            },
            Ok(Event::Text(ref e)) => {
                if in_t {
                    let span_start = pos_before;
                    let raw = e.xml10_content();
                    if let Ok(t) = quick_xml::escape::unescape(&raw) {
                        text.push_str(&t);
                    }
                    text_spans.push((span_start, bpos(reader)));
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if in_t {
                    let span_start = pos_before;
                    crate::backends::office::push_general_ref(&mut text, e);
                    text_spans.push((span_start, bpos(reader)));
                }
            }
            Ok(Event::CData(ref e)) => {
                if in_t {
                    let span_start = pos_before;
                    text.push_str(e.as_ref());
                    text_spans.push((span_start, bpos(reader)));
                }
            }
            Err(e) => return Err(DocError::corrupt(format!("XML para scan error: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    Ok(ParaSlot {
        end: bpos(reader),
        text,
        ppr,
        text_spans,
    })
}

/// 消费整个子树直到与开元素配对的 End 事件（表格等非候选元素的整棵跳过）。
fn consume_subtree<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<()> {
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

/// 消费 `<w:sdt>` 子树，把其中的正文 `w:p`（不含表格内段落）登记为候选位。
fn consume_sdt<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    xml: &str,
    slots: &mut Vec<ParaSlot>,
) -> Result<()> {
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match e.name().local_name().as_ref() {
                    tags::W_P => {
                        let slot = consume_para(reader, xml)?;
                        if slot.is_candidate() {
                            slots.push(slot);
                        }
                        // consume_para 吃掉了配对 End，深度不增减（对齐解析侧）
                        continue;
                    }
                    tags::W_TBL => {
                        // sdt 内表格：整棵跳过（表格内段落不进翻译流）
                        consume_subtree(reader)?;
                        continue;
                    }
                    _ => depth += 1,
                }
            }
            Ok(Event::End(ref _e)) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            Ok(Event::Eof) => return Ok(()),
            Err(e) => return Err(DocError::corrupt(format!("XML sdt scan error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
}

// ---------------------------------------------------------------------------
// 三种策略
// ---------------------------------------------------------------------------

/// 段落位 × 翻译单元按序配对（文本一致性守卫）+ 译文查找。
/// 返回 (slot 下标, 译文) 列表，文档序。
fn matched_translations(
    slots: &[ParaSlot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, &str>,
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let n = slots.len().min(units.len());
    for k in 0..n {
        // 双保险：位文本与单元文本必须一致（同一解析器同一文件的确定性保证）
        if slots[k].text.trim() != units[k].text.trim() {
            continue;
        }
        if let Some(tr) = map.get(units[k].id.as_str()) {
            let tr = tr.trim();
            if !tr.is_empty() {
                out.push((k, tr.to_string()));
            }
        }
    }
    out
}

/// BilingualInline：在原 `</w:p>` 后插入译文段（复制原段 pPr，文本 = 译文）。
fn apply_inline(
    xml: &str,
    slots: &[ParaSlot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, &str>,
) -> String {
    let matched = matched_translations(slots, units, map);
    if matched.is_empty() {
        return xml.to_string();
    }
    let mut edits: Vec<(usize, String)> = Vec::new();
    for (k, tr) in matched {
        let slot = &slots[k];
        let ppr = slot.ppr.clone().unwrap_or_default();
        edits.push((
            slot.end,
            format!(
                "<w:p>{ppr}<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                escape_xml_text(&tr)
            ),
        ));
    }
    splice(xml, edits)
}

/// TranslationOnly：首 `w:t` 文本换译文，其余 `w:t` 文本清空。
/// 简化说明：不删 run 结构（保留书签/字段/格式），只清文本——重解析口径
/// （段落文本 = 译文）与「首 run 换文本其余 run 删」等价，但更不易损坏文档。
fn apply_translation_only(
    xml: &str,
    slots: &[ParaSlot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, &str>,
) -> String {
    let matched = matched_translations(slots, units, map);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for (k, tr) in matched {
        let spans = &slots[k].text_spans;
        if spans.is_empty() {
            // 无文本节点可换（文本全来自 tab/br 占位）：保守跳过
            continue;
        }
        let mut it = spans.iter();
        let first = it.next().unwrap();
        edits.push((first.0, first.1, escape_xml_text(&tr)));
        for span in it {
            edits.push((span.0, span.1, String::new()));
        }
    }
    splice_spans(xml, edits)
}

/// OriginalPlusAppendix：正文不动，body 末尾追加「译文」标题段 + 译文段序列。
fn apply_appendix(
    xml: &str,
    slots: &[ParaSlot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, &str>,
    body_close: usize,
) -> String {
    let matched = matched_translations(slots, units, map);
    if matched.is_empty() {
        return xml.to_string();
    }
    let mut appendix = String::from(
        "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr>\
         <w:r><w:t>译文</w:t></w:r></w:p>",
    );
    for (_k, tr) in matched {
        appendix.push_str(&format!(
            "<w:p><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
            escape_xml_text(&tr)
        ));
    }
    let pos = appendix_insert_pos(xml, body_close);
    splice(xml, vec![(pos, appendix)])
}

/// 追加位置：body 末尾是 `<w:sectPr>`（尾块）时插在它之前，否则插在 `</w:body>` 前。
/// sectPr 若在最后一段的 pPr 里（节断符），其后必有 `</w:p>` → 落到 `</w:body>` 前。
fn appendix_insert_pos(xml: &str, body_close: usize) -> usize {
    let Some(p) = xml[..body_close].rfind("<w:sectPr") else {
        return body_close;
    };
    let tail = &xml[p..body_close];
    if tail.contains("</w:p>") || tail.contains("</w:tbl>") {
        body_close
    } else {
        p
    }
}

/// 按 (位置, 插入文本) 从后往前拼装（位置可乱序，先排序）。
fn splice(xml: &str, mut edits: Vec<(usize, String)>) -> String {
    edits.sort_by_key(|(pos, _)| *pos);
    let mut out = String::with_capacity(xml.len() + edits.iter().map(|(_, s)| s.len()).sum::<usize>());
    let mut cursor = 0usize;
    for (pos, s) in edits {
        out.push_str(&xml[cursor..pos]);
        out.push_str(&s);
        cursor = pos;
    }
    out.push_str(&xml[cursor..]);
    out
}

/// 按 (起, 止, 替换文本) 从后往前拼装（按起偏移排序，区间互不重叠）。
fn splice_spans(xml: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    edits.sort_by_key(|(s, _, _)| *s);
    let mut out = String::with_capacity(xml.len());
    let mut cursor = 0usize;
    for (s, e, txt) in edits {
        if s < cursor {
            continue; // 区间重叠（不应发生）：保守丢弃后到的
        }
        out.push_str(&xml[cursor..s]);
        out.push_str(&txt);
        cursor = e;
    }
    out.push_str(&xml[cursor..]);
    out
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
    use crate::model::BlockKind;
    use crate::model::BlockKind::{Heading, Paragraph, Table};

    /// heading + 两段（第二段仅 1 字符）+ 表格 + 尾段（含实体）。
    const DOCUMENT_XML: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>第一章 引言</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>这是第一段正文，用于测试。</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>乙</w:t></w:r></w:p>"#,
        r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>表头甲</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>表头乙</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        r#"<w:p><w:r><w:t>表格之后的段落，含 &amp; 符号。</w:t></w:r></w:p>"#,
        r#"</w:body></w:document>"#
    );

    /// 同 DOCUMENT_XML，但 body 以 sectPr 结尾（OriginalPlusAppendix 的插位考点）。
    const SECTPR_XML: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>第一章 引言</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>这是第一段正文，用于测试。</w:t></w:r></w:p>"#,
        r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>表头甲</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>表头乙</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr>"#,
        r#"</w:body></w:document>"#
    );

    const STYLES_XML: &str = r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:styleId="Heading1"><w:name w:val="heading 1"/></w:style></w:styles>"#;

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_docx_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_docx(path: &Path, document_xml: &str) {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in [
            ("word/document.xml", document_xml),
            ("word/styles.xml", STYLES_XML),
            ("[Content_Types].xml", "<Types/>"),
        ] {
            zw.start_file(name, opts).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        zw.finish().unwrap();
    }

    /// 与 writeback 同一套装配（= 提取侧 register_defaults 的行为）。
    fn parse_doc(path: &Path) -> crate::model::Document {
        let raw = DocxRawBackend::default()
            .parse(path.to_str().unwrap())
            .unwrap();
        OfficePlugin::default().assemble(&raw)
    }

    fn blocks(path: &Path) -> Vec<crate::model::Block> {
        parse_doc(path).pages[0].blocks.clone()
    }

    fn unit_ids(path: &Path) -> Vec<String> {
        to_translation_units(&parse_doc(path))
            .into_iter()
            .map(|u| u.id)
            .collect()
    }

    fn write(in_path: &Path, out_path: &Path, tr: &[(&str, &str)], strategy: BiStrategy) {
        let translated: Vec<(String, String)> =
            tr.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        writeback_docx(in_path, &translated, strategy, out_path).unwrap();
    }

    fn entry_str(zip_path: &Path, name: &str) -> String {
        let mut z = open_zip(zip_path).unwrap();
        let mut s = String::new();
        z.by_name(name)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    }

    /// 金测（核心验收）：BilingualInline 往返 —— 块序原/译交错、表格与
    /// 1 字符短段不受扰、未匹配 id 忽略、译文转义往返、其余部件字节透传。
    #[test]
    fn roundtrip_bilingual_inline() {
        let dir = case_dir("inline");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, DOCUMENT_XML);
        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "Chapter 1 Introduction"),
                ("p0-b1", "This is the first paragraph for testing."),
                ("p0-b4", "After the table & more."),
                ("p0-b99", "bogus"), // 未匹配 id：忽略
            ],
            BiStrategy::BilingualInline,
        );

        // 提取侧 id 口径（表格占 b3、短段占 b2，都不进翻译流）
        assert_eq!(
            unit_ids(&in_path),
            ["p0-b0", "p0-b1", "p0-b4"]
        );

        let got = blocks(&out_path);
        let kinds: Vec<BlockKind> = got.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                Heading, Heading, // 原标题 + 译标题（复制 Heading1 pPr → 仍是标题）
                Paragraph, Paragraph, // 原段 + 译段
                Paragraph, // 1 字符短段：不占翻译位，原样、无译邻段
                Table,     // 表格不受扰
                Paragraph, Paragraph, // 尾段原 + 译
            ]
        );
        assert_eq!(got[0].content, "第一章 引言");
        assert_eq!(got[1].content, "Chapter 1 Introduction");
        assert_eq!(got[1].level, Some(1), "译段复制 pStyle Heading1");
        assert_eq!(got[2].content, "这是第一段正文，用于测试。");
        assert_eq!(got[3].content, "This is the first paragraph for testing.");
        assert_eq!(got[4].content, "乙");
        let table = got[5].table.as_ref().unwrap();
        assert_eq!(
            table.rows,
            vec![vec!["表头甲".to_string(), "表头乙".to_string()]]
        );
        assert_eq!(got[6].content, "表格之后的段落，含 & 符号。");
        assert_eq!(got[7].content, "After the table & more.", "译文 & 转义往返");

        // 其余部件字节透传 + 条目数不变
        let mut zin = open_zip(&in_path).unwrap();
        let mut zout = open_zip(&out_path).unwrap();
        assert_eq!(zout.len(), 3);
        let mut a = String::new();
        let mut b = String::new();
        zin.by_name("word/styles.xml").unwrap().read_to_string(&mut a).unwrap();
        zout.by_name("word/styles.xml").unwrap().read_to_string(&mut b).unwrap();
        assert_eq!(a, b);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TranslationOnly：首 w:t 换译文，其余清空；未翻译段与表格原样；无插入。
    #[test]
    fn translation_only_replaces_text_keeps_rest() {
        let dir = case_dir("tonly");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, DOCUMENT_XML);
        write(
            &in_path,
            &out_path,
            &[("p0-b0", "Chapter 1"), ("p0-b4", "After the table.")],
            BiStrategy::TranslationOnly,
        );

        let got = blocks(&out_path);
        assert_eq!(got.len(), 5, "无插入：块数与原文一致");
        assert_eq!(got[0].kind, Heading);
        assert_eq!(got[0].content, "Chapter 1");
        assert_eq!(got[1].content, "这是第一段正文，用于测试。");
        assert_eq!(got[2].content, "乙");
        assert!(got[3].table.is_some());
        // 尾段文本被 &amp; 实体拆成多个 span：首 span 换译文、其余 span 清空
        assert_eq!(got[4].content, "After the table.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全部 id 未匹配：输出与原文解析结果逐块一致（透传）。
    #[test]
    fn unmatched_ids_leave_document_untouched() {
        let dir = case_dir("unmatched");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, DOCUMENT_XML);
        write(&in_path, &out_path, &[("p0-b42", "nope")], BiStrategy::BilingualInline);
        assert_eq!(blocks(&out_path), blocks(&in_path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：正文不动，body 尾 sectPr 之前追加「译文」标题 + 译文段。
    #[test]
    fn appendix_appends_before_trailing_sectpr() {
        let dir = case_dir("appendix");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, SECTPR_XML);
        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "Chapter 1 Introduction"),
                ("p0-b1", "This is the first paragraph for testing."),
            ],
            BiStrategy::OriginalPlusAppendix,
        );

        let orig = blocks(&in_path);
        let got = blocks(&out_path);
        assert_eq!(got.len(), orig.len() + 3, "追加标题 1 + 译文段 2");
        assert_eq!(got[..orig.len()], orig[..], "正文逐块不动");
        assert_eq!(got[orig.len()].kind, Heading);
        assert_eq!(got[orig.len()].content, "译文");
        assert_eq!(got[orig.len() + 1].content, "Chapter 1 Introduction");
        assert_eq!(got[orig.len() + 2].content, "This is the first paragraph for testing.");

        // XML 层：sectPr 仍在 body 末尾，且位于全部译文段之后
        let out_xml = entry_str(&out_path, "word/document.xml");
        let sect = out_xml.rfind("<w:sectPr").unwrap();
        let last_tr = out_xml.rfind("This is the first paragraph for testing.").unwrap();
        assert!(last_tr < sect, "附录插在 sectPr 之前");
        assert!(out_xml[sect..].contains("</w:body>"), "sectPr 仍收尾");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// sdt（内容控件）里的段落也是候选位；表格内段落不进翻译流。
    #[test]
    fn sdt_paragraphs_are_slots_and_tables_skipped() {
        let dir = case_dir("sdt");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        const SDT_XML: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>表甲</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
            r#"<w:sdt><w:sdtPr/><w:sdtContent><w:p><w:r><w:t>控件里的段落。</w:t></w:r></w:p></w:sdtContent></w:sdt>"#,
            r#"</w:body></w:document>"#
        );
        build_docx(&in_path, SDT_XML);

        // 表格只产 Table 块（其内 w:p 不占翻译位）；sdt 段落是 b1
        assert_eq!(unit_ids(&in_path), ["p0-b1"]);

        write(
            &in_path,
            &out_path,
            &[("p0-b1", "Slot inside content control.")],
            BiStrategy::BilingualInline,
        );
        let got = blocks(&out_path);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].kind, Table);
        assert_eq!(got[1].content, "控件里的段落。");
        assert_eq!(got[2].content, "Slot inside content control.");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
