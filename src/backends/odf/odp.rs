// 吸收自 docparser-odf 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：crate 引用重写。
//! ODP（OpenDocument Presentation）解析：`office:presentation` 下每个 `draw:page` 一页。
//!
//! 形状（`draw:frame` / `draw:text-box`）内的 `text:p` 递归收集；页内表格结构化输出。
//! 页名取 `draw:name`，缺省用 `Slide N`（与 pptx 后端的命名口径一致）。

use std::path::Path;

use crate::error::{DocError, Result};
use crate::plugin::{ParseCapability, RawDoc, RawPage, RawTextItem};
use quick_xml::events::Event;
use quick_xml::Reader;

/// ODP raw 后端。
pub struct OdpRawBackend;

impl ParseCapability for OdpRawBackend {
    fn id(&self) -> &str {
        "odf-raw"
    }
    fn formats(&self) -> &[&str] {
        &["odp"]
    }
    fn parse(&self, path: &Path) -> Result<RawDoc> {
        let xml = super::read_content_xml(path)?;
        Ok(RawDoc {
            pages: odp_pages(&xml)?,
            ..Default::default()
        })
    }
}

/// 把 ODP 的 `content.xml` 解析成「每张幻灯片一页」。
fn odp_pages(xml: &str) -> Result<Vec<RawPage>> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();

    // 定位 <office:presentation>
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e))
                if e.name().local_name().as_ref() == super::tags::PRESENTATION =>
            {
                break
            }
            Ok(Event::Eof) => {
                return Err(DocError::corrupt(
                    "no office:presentation found（不是 ODP）",
                ))
            }
            Err(e) => return Err(DocError::corrupt(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    buf.clear();

    let mut pages: Vec<RawPage> = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().local_name().as_ref() == super::tags::DRAW_PAGE {
                    let name = super::attr(e, super::tags::NAME)
                        .or_else(|| Some(format!("Slide {}", pages.len() + 1)));
                    let (items, tables) = page_content(&mut reader)?;
                    pages.push(RawPage {
                        name,
                        items,
                        tables,
                        ..Default::default()
                    });
                } else {
                    depth += 1;
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

    Ok(pages)
}

/// 收集单页内的段落与表格（调用前已消费 `<draw:page>` 的 Start）。
#[allow(clippy::type_complexity)]
fn page_content<R: std::io::BufRead>(
    reader: &mut Reader<R>,
) -> Result<(Vec<RawTextItem>, Vec<Vec<Vec<String>>>)> {
    let mut items: Vec<RawTextItem> = Vec::new();
    let mut tables: Vec<Vec<Vec<String>>> = Vec::new();
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().local_name();
                match name.as_ref() {
                    super::tags::P => {
                        let t = super::text_until(reader, super::tags::P)?;
                        let trimmed = t.trim();
                        if !trimmed.is_empty() {
                            items.push(RawTextItem {
                                text: trimmed.to_string(),
                                ..Default::default()
                            });
                        }
                    }
                    super::tags::TABLE => {
                        let rows = super::table_rows(reader)?;
                        if !rows.is_empty() {
                            tables.push(rows);
                        }
                    }
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
    Ok((items, tables))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"><office:body><office:presentation>"#;
    const TAIL: &str = "</office:presentation></office:body></office:document-content>";

    fn pages(body: &str) -> Vec<RawPage> {
        odp_pages(&format!("{HEAD}{body}{TAIL}")).unwrap()
    }

    #[test]
    fn each_page_becomes_one_slide() {
        let ps = pages(
            r#"<draw:page draw:name="封面"><draw:frame><draw:text-box><text:p>标题</text:p><text:p>副标题</text:p></draw:text-box></draw:frame></draw:page><draw:page draw:name="第二页"><draw:frame><draw:text-box><text:p>正文</text:p></draw:text-box></draw:frame></draw:page>"#,
        );
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].name.as_deref(), Some("封面"));
        assert_eq!(ps[0].items.len(), 2);
        assert_eq!(ps[0].items[1].text, "副标题");
        assert_eq!(ps[1].name.as_deref(), Some("第二页"));
    }

    #[test]
    fn missing_page_name_gets_indexed_fallback() {
        let ps = pages(
            r#"<draw:page><draw:frame><draw:text-box><text:p>x</text:p></draw:text-box></draw:frame></draw:page>"#,
        );
        assert_eq!(ps[0].name.as_deref(), Some("Slide 1"));
    }

    #[test]
    fn table_inside_page_is_structured() {
        let ps = pages(
            r#"<draw:page draw:name="S"><draw:frame><draw:text-box><table:table xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><table:table-row><table:table-cell><text:p>A</text:p></table:table-cell><table:table-cell><text:p>B</text:p></table:table-cell></table:table-row></table:table></draw:text-box></draw:frame></draw:page>"#,
        );
        assert_eq!(ps[0].tables.len(), 1);
        assert_eq!(ps[0].tables[0][0], vec!["A", "B"]);
    }
}
