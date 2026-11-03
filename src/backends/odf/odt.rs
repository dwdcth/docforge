// 吸收自 docparser-odf 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：crate 引用重写。
//! ODT（OpenDocument Text）解析：`content.xml` 的 `office:text` 下逐段提取。
//!
//! raw 模式口径对齐 `docparser-office` 的 docx：段落逐条（`text:h` 带 `text:outline-level`
//! 还原为标题），表格额外产出结构化 `TableData`；列表按 raw 处理（只留文本，不还原层级）。

use crate::error::{DocError, Result};
use crate::plugin::{ParseCapability, RawDoc, RawPage, RawTextItem};
use quick_xml::events::Event;
use quick_xml::Reader;

/// ODT raw 后端。
pub struct OdtRawBackend;

impl ParseCapability for OdtRawBackend {
    fn id(&self) -> &str {
        "odf-raw"
    }
    fn formats(&self) -> &[&str] {
        &["odt"]
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let xml = super::read_content_xml(path)?;
        Ok(RawDoc {
            pages: vec![odt_raw_page(&xml)?],
            ..Default::default()
        })
    }
}

fn push_item(items: &mut Vec<RawTextItem>, text: String, level: Option<u8>) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        items.push(RawTextItem {
            text: trimmed.to_string(),
            level,
            ..Default::default()
        });
    }
}

/// 把 ODT 的 `content.xml` 解析成单页 RawPage（ODT 无物理页概念，与 DOCX 同语义）。
fn odt_raw_page(xml: &str) -> Result<RawPage> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();

    // 定位 <office:text>（office:body 的唯一字处理子节点）
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().local_name().as_ref() == super::tags::TEXT => break,
            Ok(Event::Eof) => return Err(DocError::corrupt("no office:text found（不是 ODT）")),
            Err(e) => return Err(DocError::corrupt(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    buf.clear();

    let mut items: Vec<RawTextItem> = Vec::new();
    let mut tables: Vec<Vec<Vec<String>>> = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().local_name();
                match name.as_ref() {
                    super::tags::H => {
                        // 标题标签本身就是标题语义：`text:outline-level` 缺失/非法时按 1 级
                        // （之前落到 None 会被 assemble 降级成 Paragraph，与 Python 侧不一致）
                        let lv = super::attr(e, super::tags::OUTLINE_LEVEL)
                            .and_then(|v| v.trim().parse::<u8>().ok())
                            .or(Some(1));
                        let t = super::text_until(&mut reader, super::tags::H)?;
                        push_item(&mut items, t, lv);
                    }
                    super::tags::P => {
                        let t = super::text_until(&mut reader, super::tags::P)?;
                        push_item(&mut items, t, None);
                    }
                    super::tags::TABLE => {
                        let rows = super::table_rows(&mut reader)?;
                        if !rows.is_empty() {
                            tables.push(rows);
                        }
                    }
                    // list / section / tracked-changes 等：递归进去，内部的 text:p 照常收集
                    _ => depth += 1,
                }
            }
            Ok(Event::End(ref _e)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    Ok(RawPage {
        name: None,
        items,
        tables,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:text>"#;
    const TAIL: &str = "</office:text></office:body></office:document-content>";

    fn page(body: &str) -> RawPage {
        odt_raw_page(&format!("{HEAD}{body}{TAIL}")).unwrap()
    }

    #[test]
    fn paragraphs_and_headings() {
        let p = page(
            r#"<text:h text:outline-level="2">第二章</text:h><text:p>正文一</text:p><text:p>正文二</text:p>"#,
        );
        assert_eq!(p.items.len(), 3);
        assert_eq!(p.items[0].text, "第二章");
        assert_eq!(p.items[0].level, Some(2));
        assert_eq!(p.items[1].text, "正文一");
        assert_eq!(p.items[1].level, None);
    }

    #[test]
    fn heading_without_outline_level_defaults_to_one() {
        // 缺 `text:outline-level` 不能退化成正文段落（assemble 只认 level 判 Heading）
        let p = page(r#"<text:h>无级标题</text:h>"#);
        assert_eq!(p.items[0].level, Some(1));
    }

    #[test]
    fn spaces_tabs_and_line_breaks() {
        let p = page(r#"<text:p>a<text:s text:c="3"/>b<text:tab/>c<text:line-break/>d</text:p>"#);
        assert_eq!(p.items[0].text, "a   b\tc\nd");
    }

    #[test]
    fn table_with_repeated_cells() {
        let p = page(
            r#"<table:table table:name="T1"><table:table-row><table:table-cell><text:p>A</text:p></table:table-cell><table:table-cell table:number-columns-repeated="2"><text:p>B</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>1</text:p></table:table-cell><table:table-cell/><table:table-cell><text:p>3</text:p></table:table-cell></table:table-row></table:table>"#,
        );
        assert_eq!(p.tables.len(), 1);
        assert_eq!(p.tables[0][0], vec!["A", "B", "B"]);
        assert_eq!(p.tables[0][1], vec!["1", "", "3"]);
    }

    #[test]
    fn list_items_are_flattened_to_paragraphs() {
        let p = page(
            r#"<text:list><text:list-item><text:p>甲</text:p></text:list-item><text:list-item><text:p>乙</text:p></text:list-item></text:list>"#,
        );
        let texts: Vec<&str> = p.items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, vec!["甲", "乙"]);
    }

    #[test]
    fn entity_refs_are_decoded() {
        let p = page(r#"<text:p>a &amp; b &#20013;</text:p>"#);
        assert_eq!(p.items[0].text, "a & b 中");
    }

    #[test]
    fn non_odt_content_is_corrupt() {
        let err = odt_raw_page("<html><body>x</body></html>").unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
    }
}
