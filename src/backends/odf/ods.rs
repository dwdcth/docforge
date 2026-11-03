// 吸收自 docparser-odf 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：crate 引用重写。
//! ODS（OpenDocument Spreadsheet）解析：`office:spreadsheet` 下每张 `table:table` 一页。
//!
//! 单元格取值优先级：`text:p` 里的显示文本 → `office:value` / `office:date-value` /
//! `office:time-value` / `office:boolean-value` / `office:string-value`（显示文本为空时兜底）。
//! `table:number-columns-repeated` / `number-rows-repeated` 按次数展开（上限见
//! `MAX_REPEAT` = 1024），这样空列/空行的压缩表示不会吃掉内存。

use crate::error::{DocError, Result};
use crate::plugin::{ParseCapability, RawDoc, RawPage};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// 重新计算时可能出现的超大行数（Excel 上限量级）——超出截断，避免病理文件撑爆内存。
const MAX_ROWS: usize = 65_536;

/// ODS raw 后端。
pub struct OdsRawBackend;

impl ParseCapability for OdsRawBackend {
    fn id(&self) -> &str {
        "odf-raw"
    }
    fn formats(&self) -> &[&str] {
        &["ods"]
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let xml = super::read_content_xml(path)?;
        Ok(RawDoc {
            pages: ods_pages(&xml)?,
            ..Default::default()
        })
    }
}

/// 单元格的兜底取值（显示文本为空时用）。
fn cell_value(e: &BytesStart<'_>) -> Option<String> {
    for key in [
        super::tags::DATE_VALUE,
        super::tags::TIME_VALUE,
        super::tags::BOOLEAN_VALUE,
        super::tags::STRING_VALUE,
        super::tags::VALUE,
    ] {
        if let Some(v) = super::attr(e, key) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// 把 ODS 的 `content.xml` 解析成「每张表一页」。
fn ods_pages(xml: &str) -> Result<Vec<RawPage>> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();

    // 定位 <office:spreadsheet>
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e))
                if e.name().local_name().as_ref() == super::tags::SPREADSHEET =>
            {
                break
            }
            Ok(Event::Eof) => {
                return Err(DocError::corrupt("no office:spreadsheet found（不是 ODS）"))
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
                if e.name().local_name().as_ref() == super::tags::TABLE {
                    let name = super::attr(e, super::tags::NAME);
                    let rows = sheet_rows(&mut reader)?;
                    if !rows.is_empty() {
                        pages.push(RawPage {
                            name,
                            tables: vec![rows],
                            ..Default::default()
                        });
                    }
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

/// 读取一张表（调用前已消费 `<table:table>` 的 Start），返回 rows × cells。
fn sheet_rows<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<Vec<Vec<String>>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut in_row = false;
    let mut row_repeat = 1usize;
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().local_name();
                match name.as_ref() {
                    super::tags::TABLE_ROW => {
                        in_row = true;
                        row.clear();
                        row_repeat = super::repeat_of(e, super::tags::ROWS_REPEATED);
                        depth += 1;
                    }
                    super::tags::TABLE_CELL | super::tags::COVERED_CELL => {
                        if in_row {
                            let text = super::text_until(reader, name.as_ref())?;
                            let value = if text.trim().is_empty() {
                                cell_value(e).unwrap_or_default()
                            } else {
                                text.trim().to_string()
                            };
                            for _ in 0..super::repeat_of(e, super::tags::COLS_REPEATED) {
                                row.push(value.clone());
                            }
                        } else {
                            // 不在行内的游离单元格：跳过其内容
                            super::text_until(reader, name.as_ref())?;
                        }
                    }
                    _ => depth += 1,
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.name().local_name();
                if matches!(
                    name.as_ref(),
                    super::tags::TABLE_CELL | super::tags::COVERED_CELL
                ) && in_row
                {
                    let value = cell_value(e).unwrap_or_default();
                    for _ in 0..super::repeat_of(e, super::tags::COLS_REPEATED) {
                        row.push(value.clone());
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name().local_name();
                if name.as_ref() == super::tags::TABLE_ROW && in_row {
                    push_row(&mut rows, &mut row, row_repeat);
                    row_repeat = 1;
                    in_row = false;
                }
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML table parse error: {e}"))),
            Ok(_) => {}
        }
        buf.clear();
        if rows.len() >= MAX_ROWS {
            break;
        }
    }

    Ok(rows)
}

fn push_row(rows: &mut Vec<Vec<String>>, row: &mut Vec<String>, repeat: usize) {
    for i in 0..repeat.max(1) {
        if i + 1 == repeat.max(1) {
            rows.push(std::mem::take(row));
        } else {
            rows.push(row.clone());
        }
        if rows.len() >= MAX_ROWS {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"><office:body><office:spreadsheet>"#;
    const TAIL: &str = "</office:spreadsheet></office:body></office:document-content>";

    fn pages(body: &str) -> Vec<RawPage> {
        ods_pages(&format!("{HEAD}{body}{TAIL}")).unwrap()
    }

    #[test]
    fn each_sheet_becomes_one_page() {
        let ps = pages(
            r#"<table:table table:name="一月"><table:table-row><table:table-cell><text:p>1</text:p></table:table-cell></table:table-row></table:table><table:table table:name="二月"><table:table-row><table:table-cell><text:p>2</text:p></table:table-cell></table:table-row></table:table>"#,
        );
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].name.as_deref(), Some("一月"));
        assert_eq!(ps[1].tables[0][0], vec!["2"]);
    }

    #[test]
    fn repeated_columns_and_rows_are_expanded() {
        let ps = pages(
            r#"<table:table table:name="S"><table:table-row table:number-rows-repeated="2"><table:table-cell><text:p>A</text:p></table:table-cell><table:table-cell table:number-columns-repeated="2"><text:p>B</text:p></table:table-cell></table:table-row></table:table>"#,
        );
        let rows = &ps[0].tables[0];
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["A", "B", "B"]);
        assert_eq!(rows[1], vec!["A", "B", "B"]);
    }

    #[test]
    fn value_attributes_are_fallback_when_text_is_empty() {
        let ps = pages(
            r#"<table:table table:name="S"><table:table-row><table:table-cell office:value-type="float" office:value="42"><text:p/></table:table-cell><table:table-cell office:value-type="date" office:date-value="2026-10-24"><text:p/></table:table-cell><table:table-cell office:value-type="boolean" office:boolean-value="true"><text:p/></table:table-cell></table:table-row></table:table>"#,
        );
        assert_eq!(ps[0].tables[0][0], vec!["42", "2026-10-24", "true"]);
    }

    #[test]
    fn covered_cells_and_empty_rows_are_kept_as_blanks() {
        let ps = pages(
            r#"<table:table table:name="S"><table:table-row><table:table-cell><text:p>A</text:p></table:table-cell><table:covered-table-cell/></table:table-row></table:table>"#,
        );
        assert_eq!(ps[0].tables[0][0], vec!["A", ""]);
    }

    #[test]
    fn pathological_repeat_is_capped() {
        let ps = pages(
            r#"<table:table table:name="S"><table:table-row table:number-rows-repeated="1048576"><table:table-cell/></table:table-row></table:table>"#,
        );
        // MAX_REPEAT 截断到 1024 行，不会因为压缩表示撑爆内存
        assert_eq!(ps[0].tables[0].len(), super::super::MAX_REPEAT);
    }
}
