// 吸收自 docparser-odf 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改头：作为 docforge::backends::odf 模块；crate 引用重写。
//! docparser ODF 后端：ODT / ODS / ODP 解析（`zip` + `quick-xml` 流式）。
//!
//! ODF（OpenDocument Format）与 OOXML 结构同构：ZIP 容器 + 一份 `content.xml`，
//! 正文分别落在 `office:text`（字处理）/ `office:spreadsheet`（表格）/
//! `office:presentation`（演示）下。本 crate 复用 `docparser-office` 的 raw 模式语义：
//! 段落逐条输出，表格额外产出结构化 `TableData`；标题保留 `text:outline-level`。
//!
//! 与 Office 后端的差异：
//!   * 命名空间是 `urn:oasis:names:tc:opendocument:xmlns:*`（ODF 1.2）；
//!   * 空格/制表/换行是显式元素（`text:s` / `text:tab` / `text:line-break`），
//!     没有 OOXML 那种 `xml:space="preserve"` 口径；
//!   * 表格有 `table:number-columns-repeated` / `number-rows-repeated` 压缩表示。
//!
//! 按文件类型裁剪：`odt` / `ods` / `odp` 三个 feature，默认全开。

use std::sync::Arc;

#[cfg(any(feature = "odt", feature = "ods", feature = "odp"))]
use crate::error::{DocError, Result};
use crate::model::{Block, BlockKind, Document, Page, TableData};
use crate::plugin::{FormatPlugin, RawDoc};
use crate::registry::Registry;

#[cfg(feature = "odp")]
pub mod odp;
#[cfg(feature = "ods")]
pub mod ods;
#[cfg(feature = "odt")]
pub mod odt;

/// 本 crate 实际编译进来的格式列表（随 feature 变化）。
pub const FORMATS: &[&str] = &[
    #[cfg(feature = "odt")]
    "odt",
    #[cfg(feature = "ods")]
    "ods",
    #[cfg(feature = "odp")]
    "odp",
];

/// ODF 命名空间前缀 → URI（文档用；解析时按本地名比较，不依赖前缀）。
pub mod ns {
    pub const OFFICE: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
    pub const TEXT: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
    pub const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
    pub const DRAW: &str = "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0";
}

/// ODF 元素 / 属性**本地名**（去前缀；比较一律用 `Name::local_name()`）。
pub mod tags {
    // 通用
    pub const BODY: &str = "body";
    pub const CONTENT: &str = "document-content";
    // 三个正文容器
    pub const TEXT: &str = "text";
    pub const SPREADSHEET: &str = "spreadsheet";
    pub const PRESENTATION: &str = "presentation";
    // 文本
    pub const H: &str = "h";
    pub const P: &str = "p";
    pub const S: &str = "s";
    pub const TAB: &str = "tab";
    pub const LINE_BREAK: &str = "line-break";
    pub const LIST: &str = "list";
    pub const LIST_ITEM: &str = "list-item";
    // 表格
    pub const TABLE: &str = "table";
    pub const TABLE_ROW: &str = "table-row";
    pub const TABLE_CELL: &str = "table-cell";
    pub const COVERED_CELL: &str = "covered-table-cell";
    // 演示
    pub const DRAW_PAGE: &str = "page";
    // 属性
    pub const OUTLINE_LEVEL: &str = "outline-level";
    pub const C: &str = "c";
    pub const NAME: &str = "name";
    pub const COLS_REPEATED: &str = "number-columns-repeated";
    pub const ROWS_REPEATED: &str = "number-rows-repeated";
    pub const VALUE_TYPE: &str = "value-type";
    pub const VALUE: &str = "value";
    pub const DATE_VALUE: &str = "date-value";
    pub const TIME_VALUE: &str = "time-value";
    pub const BOOLEAN_VALUE: &str = "boolean-value";
    pub const STRING_VALUE: &str = "string-value";
}

/// 重复单元上限：`table:number-*-repeated="1048576"` 这类写法（空行/空列压缩）会
/// 让朴素展开直接吃满内存，这里封顶，超出部分截断（与 Excel 上限同量级）。
#[cfg(any(feature = "odt", feature = "ods", feature = "odp"))]
const MAX_REPEAT: usize = 1024;

/// ODF 插件：RawDoc → IR Document 组装。
///
/// 与 Office 后端的唯一差别：这里会把 `RawTextItem.level` 还原成 `BlockKind::Heading`，
/// 并把 `RawPage.tables` 还原成 `BlockKind::Table`（带 `TableData`）。
pub struct OdfPlugin;

impl FormatPlugin for OdfPlugin {
    fn id(&self) -> &str {
        "odf"
    }
    fn formats(&self) -> &[&str] {
        FORMATS
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["odf-raw"]
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        vec![]
    }
    fn assemble(&self, raw: &RawDoc) -> Document {
        let pages: Vec<Page> = raw
            .pages
            .iter()
            .map(|rp| {
                let mut blocks: Vec<Block> = Vec::new();
                for it in &rp.items {
                    let text = it.text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let mut block = match it.level {
                        Some(lv) => Block {
                            kind: BlockKind::Heading,
                            content: text.to_string(),
                            level: Some(lv.clamp(1, 6)),
                            ..Default::default()
                        },
                        None => Block {
                            kind: BlockKind::Paragraph,
                            content: text.to_string(),
                            ..Default::default()
                        },
                    };
                    // 与 office 组装同口径：raw 项带的元信息原样进块
                    block.meta = it.meta.clone();
                    blocks.push(block);
                }
                for rows in &rp.tables {
                    if rows.is_empty() {
                        continue;
                    }
                    // 表格块同时保留纯文本形态（与 OOXML raw 口径一致：单元格逗号、行换行）
                    let content = rows
                        .iter()
                        .map(|r| r.join(","))
                        .collect::<Vec<_>>()
                        .join("\n");
                    blocks.push(Block {
                        kind: BlockKind::Table,
                        content,
                        table: Some(TableData {
                            rows: rows.clone(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                }
                // 块之间用空行分隔，与 office 后端（docx/xlsx/pptx 的 `parts.join("\n\n")`）
                // 以及 Python `blocks_to_text` 默认分隔符保持一致；此前这里是单换行，
                // 会让同一份「标题+正文」在 odt 与 docx 里落成不同文本形态。
                let text = blocks
                    .iter()
                    .map(|b| b.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                Page {
                    name: rp.name.clone(),
                    blocks,
                    text,
                    ..Default::default()
                }
            })
            .collect();
        Document {
            pages,
            ..Default::default()
        }
    }
}

/// 注册 ODF 后端与插件。只注册编译进来的格式（见 [`FORMATS`]）。
pub fn register_defaults(reg: &mut Registry) {
    #[cfg(feature = "odt")]
    reg.register_parse(Arc::new(odt::OdtRawBackend), None).ok();
    #[cfg(feature = "ods")]
    reg.register_parse(Arc::new(ods::OdsRawBackend), None).ok();
    #[cfg(feature = "odp")]
    reg.register_parse(Arc::new(odp::OdpRawBackend), None).ok();
    reg.register_plugin(Arc::new(OdfPlugin));
}

/// 从 ZIP 中读取字符串条目（找不到返回 None）。
#[cfg(any(feature = "odt", feature = "ods", feature = "odp"))]
pub(crate) fn zip_read_str(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
) -> Result<Option<String>> {
    match zip.by_name(name) {
        Ok(mut f) => {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut buf)
                .map_err(|e| DocError::corrupt(format!("读取 {name} 失败: {e}")))?;
            Ok(Some(String::from_utf8(buf).map_err(|e| {
                DocError::corrupt(format!("{name} 不是 UTF-8: {e}"))
            })?))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(DocError::corrupt(format!("ZIP 读取 {name} 失败: {e}"))),
    }
}

/// 打开 ODF 文件并读出 `content.xml`（缺该条目即判定损坏）。
#[cfg(any(feature = "odt", feature = "ods", feature = "odp"))]
pub(crate) fn read_content_xml(path: &str) -> Result<String> {
    let file = std::fs::File::open(path)
        .map_err(|e| DocError::corrupt(format!("cannot open {path}: {e}")))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| DocError::corrupt(format!("ZIP open failed: {e}")))?;
    zip_read_str(&mut zip, "content.xml")?
        .ok_or_else(|| DocError::corrupt("missing content.xml（不是 ODF 包）"))
}

/// 取属性值（按**本地名**匹配，忽略命名空间前缀）。
#[cfg(any(feature = "odt", feature = "ods", feature = "odp"))]
pub(crate) fn attr(e: &quick_xml::events::BytesStart<'_>, local: &str) -> Option<String> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.local_name().as_ref() == local)
        .map(|a| a.value.to_string())
}

/// 重复次数属性解析（`text:c` / `table:number-*-repeated`），缺省 1，超 [`MAX_REPEAT`] 截断。
#[cfg(any(feature = "odt", feature = "ods", feature = "odp"))]
pub(crate) fn repeat_of(e: &quick_xml::events::BytesStart<'_>, local: &str) -> usize {
    attr(e, local)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_REPEAT)
}

/// quick-xml 0.42 把 `&amp;` / `&#65;` 拆成独立 `Event::GeneralRef`；按 0.37 语义就地解码。
#[cfg(any(feature = "odt", feature = "ods", feature = "odp"))]
pub(crate) fn push_general_ref(buf: &mut String, e: &quick_xml::events::BytesRef<'_>) {
    match e.resolve_char_ref() {
        Ok(Some(ch)) => buf.push(ch),
        _ => {
            let entity = format!("&{};", e.xml10_content());
            if let Ok(t) = quick_xml::escape::unescape(&entity) {
                buf.push_str(&t);
            }
        }
    }
}

/// 读文本直到与 `end` 匹配的 `</...>`（含嵌套），把 `text:s` / `text:tab` /
/// `text:line-break` 转成空格 / 制表 / 换行，其余标签透明。
#[cfg(any(feature = "odt", feature = "ods", feature = "odp"))]
pub(crate) fn text_until<R: std::io::BufRead>(
    reader: &mut quick_xml::Reader<R>,
    end: &str,
) -> Result<String> {
    use quick_xml::events::Event;

    let mut out = String::new();
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name().local_name().as_ref() {
                tags::S => out.push_str(&" ".repeat(repeat_of(e, tags::C))),
                tags::TAB => out.push('\t'),
                tags::LINE_BREAK => out.push('\n'),
                _ => depth += 1,
            },
            Ok(Event::Empty(ref e)) => match e.name().local_name().as_ref() {
                tags::S => out.push_str(&" ".repeat(repeat_of(e, tags::C))),
                tags::TAB => out.push('\t'),
                tags::LINE_BREAK => out.push('\n'),
                _ => {}
            },
            Ok(Event::End(ref e)) => {
                if e.name().local_name().as_ref() == end {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                } else {
                    depth = depth.saturating_sub(1);
                }
            }
            Ok(Event::Text(ref e)) => {
                // 纯缩进空白节点丢弃（ODF 的显式空格走 text:s）
                let raw = e.xml10_content();
                if !raw.trim().is_empty() {
                    if let Ok(t) = quick_xml::escape::unescape(&raw) {
                        out.push_str(&t);
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => push_general_ref(&mut out, e),
            Ok(Event::CData(ref e)) => out.push_str(e.as_ref()),
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// 通用表格提取（ODT / ODP 共用）：调用前已消费 `<table:table>` 的 Start。
///
/// 返回 rows × cells；`table:number-columns-repeated` 按次数展开，空单元格保留为 ""。
#[cfg(any(feature = "odt", feature = "odp"))]
pub(crate) fn table_rows<R: std::io::BufRead>(
    reader: &mut quick_xml::Reader<R>,
) -> Result<Vec<Vec<String>>> {
    use quick_xml::events::Event;

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut in_row = false;
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().local_name();
                match name.as_ref() {
                    tags::TABLE_CELL | tags::COVERED_CELL => {
                        // 单元格内的一切（含 text:p、嵌套 span）都拍平成一段文本
                        let cell = text_until(reader, name.as_ref())?;
                        if in_row {
                            for _ in 0..repeat_of(e, tags::COLS_REPEATED) {
                                row.push(cell.trim().to_string());
                            }
                        }
                    }
                    tags::TABLE_ROW => {
                        in_row = true;
                        row.clear();
                        depth += 1;
                    }
                    _ => depth += 1,
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.name().local_name();
                if matches!(name.as_ref(), tags::TABLE_CELL | tags::COVERED_CELL) && in_row {
                    for _ in 0..repeat_of(e, tags::COLS_REPEATED) {
                        row.push(String::new());
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name().local_name();
                if name.as_ref() == tags::TABLE_ROW && in_row {
                    rows.push(std::mem::take(&mut row));
                    in_row = false;
                }
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML table parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::FORMATS;

    /// `FORMATS` 必须与编译进来的 feature 严格一致。
    #[test]
    fn formats_follow_features() {
        let mut want: Vec<&str> = Vec::new();
        if cfg!(feature = "odt") {
            want.push("odt");
        }
        if cfg!(feature = "ods") {
            want.push("ods");
        }
        if cfg!(feature = "odp") {
            want.push("odp");
        }
        assert_eq!(FORMATS, want.as_slice());
    }

    /// 端到端：真 ZIP 包 → 后端解析 → 插件组装（标题还原 Heading、表格还原 Table）。
    #[cfg(feature = "odt")]
    #[test]
    fn odt_zip_roundtrip_produces_heading_and_table_blocks() {
        use crate::model::BlockKind;
        use crate::plugin::ParseCapability;
        use std::io::Write;

        const CONTENT: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0""#,
            r#" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0""#,
            r#" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0">"#,
            r#"<office:body><office:text>"#,
            r#"<text:h text:outline-level="1">标题一</text:h>"#,
            r#"<text:p>正文段落</text:p>"#,
            r#"<table:table table:name="T"><table:table-row>"#,
            r#"<table:table-cell><text:p>A</text:p></table:table-cell>"#,
            r#"<table:table-cell><text:p>B</text:p></table:table-cell>"#,
            r#"</table:table-row></table:table>"#,
            r#"</office:text></office:body></office:document-content>"#
        );

        let dir = std::env::temp_dir().join("docforge_smoke");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("min.odt");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            zw.start_file(
                "mimetype",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            zw.write_all(b"application/vnd.oasis.opendocument.text")
                .unwrap();
            zw.start_file("content.xml", zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(CONTENT.as_bytes()).unwrap();
            zw.finish().unwrap();
        }

        let raw = crate::backends::odf::odt::OdtRawBackend
            .parse(path.to_str().unwrap())
            .unwrap();
        std::fs::remove_file(&path).ok();

        let doc = super::OdfPlugin.assemble(&raw);
        use crate::plugin::FormatPlugin;
        assert_eq!(doc.pages.len(), 1);
        let kinds: Vec<BlockKind> = doc.pages[0].blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![BlockKind::Heading, BlockKind::Paragraph, BlockKind::Table]
        );
        assert_eq!(doc.pages[0].blocks[0].level, Some(1));
        assert_eq!(
            doc.pages[0].blocks[2].table.as_ref().unwrap().rows[0],
            vec!["A", "B"]
        );
    }
}
