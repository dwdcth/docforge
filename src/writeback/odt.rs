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
//!   `text_until` 拍平，重解析口径等价）。
//! * 表格格级回写（wave2）：`table:table` 进表格模式——逐 `table:table-row`
//!   （每个行元素占一行；`table:number-rows-repeated` 解析侧 `table_rows`
//!   不展开，镜像不展开）→ 逐 `table:table-cell` / `table:covered-table-cell`
//!   （列游标按 `table:number-columns-repeated` 展开推进，covered 格照常
//!   占列；`table:number-columns-spanned` 解析侧不读，同样忽略）。格文本 =
//!   格内文本拍平拼接（`text_until` 同口径，多段落直接相连），trim 非空
//!   占一个单元位、配对下一个 `kind: Table` 单元（`p0-b{块}-r{行}c{列}`），
//!   空格不占位；重复格（cols-repeated > 1）照常占满单元位但一律不回写
//!   ——一个 XML 元素承载不了多份格译文，宁漏勿错。
//! * 三策略格内口径：TranslationOnly 把格内容区整体替换为单个
//!   `<text:p>译文</text:p>`（格内容必须是块级段落元素，裸文本会产出
//!   不合 schema 的 ODF；重解析口径与段落级「元素内文本区整体替换」
//!   等价）；BilingualInline 在格内最后一个 `text:p` 之后追加 `<text:p>`
//!   （沿用该段 `text:style-name`——「text:p 之后插 text:p」对其父容器
//!   结构恒合法）；OriginalPlusAppendix 表格不进附录、原样；未命中格
//!   原样。
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
/// [`crate::emit::to_translation_units`] 同口径（段落类 `p0-b{块序}`、格单元
/// `p0-b{块序}-r{行}c{列}`）；未匹配 / 空白译文条目忽略；失败返回 `corrupt`。
pub fn writeback_odt(
    in_path: impl AsRef<Path>,
    translated: &[(String, String)],
    strategy: BiStrategy,
    out: impl AsRef<Path>,
) -> Result<()> {
    let in_path = in_path.as_ref();
    let out = out.as_ref();

    // A. 权威解析（提取侧同管线）拿 id / 文本（全量单元流：段落类单元随
    // 段落块就地配对，格单元随表格块就地配对）
    let raw = OdtRawBackend.parse(in_path)?;
    let doc = OdfPlugin.assemble(&raw);
    let units = crate::emit::to_translation_units(&doc);
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
    // 表格数守卫：装配侧每张非空行表产一个 Table 块，扫描侧同口径收表
    let table_blocks = page
        .blocks
        .iter()
        .filter(|b| b.kind == crate::model::BlockKind::Table)
        .count();
    if table_blocks != scanned.tables.len() {
        return Err(DocError::corrupt(
            "odt 对齐失败：重扫描表格数与解析表格数不一致",
        ));
    }

    // C. 块 × 单元按序配对（同一过滤口径）+ 一致性守卫，逐策略产编辑
    let mut edits: Vec<Edit> = Vec::new();
    let mut matched: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    let mut table_cursor = 0usize;
    for (b_idx, b) in page.blocks.iter().enumerate() {
        if b.kind == crate::model::BlockKind::Table {
            // 表格块：格位（扫描序 = 解析侧网格 r 优先序 = 单元流序）× 格
            // 单元按序配对；空格不产单元也不占位，重复格占位不回写（见模块头）
            let cells = &scanned.tables[table_cursor];
            table_cursor += 1;
            for cell in cells {
                if cursor >= units.len() || units[cursor].kind != crate::model::BlockKind::Table {
                    break; // 单元流耗尽 / 错位：宁漏勿错
                }
                let unit = &units[cursor];
                cursor += 1;
                // 双保险：重扫描格文本必须与权威单元文本一致
                if cell.text != unit.text {
                    continue;
                }
                // 重复格（cols-repeated > 1）：占单元位但不回写（见 consume_table）
                if !cell.editable {
                    continue;
                }
                let Some(tr) = map.get(unit.id.as_str()) else {
                    continue;
                };
                let tr = tr.trim();
                if tr.is_empty() {
                    continue;
                }
                match strategy {
                    BiStrategy::BilingualInline => {
                        let Some(after) = cell.last_para_after else {
                            continue; // 格内无 text:p 锚点：宁漏勿错
                        };
                        let style = cell
                            .last_para_style
                            .as_deref()
                            .map(|s| format!(" text:style-name=\"{s}\""))
                            .unwrap_or_default();
                        edits.push(Edit::Insert(
                            after,
                            format!("<text:p{style}>{}</text:p>", escape_xml_text(tr)),
                        ));
                    }
                    BiStrategy::TranslationOnly => {
                        // 格内容区整体替换为单个译段（格内容必须是块级元素）
                        let (s, e) = cell.inner;
                        if s < e {
                            edits.push(Edit::Replace(
                                s,
                                e,
                                format!("<text:p>{}</text:p>", escape_xml_text(tr)),
                            ));
                        }
                    }
                    // 表格不进附录、原样（格译文无处安放）
                    BiStrategy::OriginalPlusAppendix => {}
                }
            }
            continue;
        }
        let t = b.content.trim();
        if !crate::emit::is_translatable(b.kind) || t.is_empty() || t.chars().count() < 2 {
            continue; // 表格块（装配时统一排在段后）在此自然跳过
        }
        if b_idx >= scanned.slots.len() || cursor >= units.len() {
            break;
        }
        let unit = &units[cursor];
        if unit.kind == crate::model::BlockKind::Table {
            break; // 表格单元未耗尽（扫描/解析错位）：后续全部保守放弃
        }
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

/// 一个表格格的重扫描定位（trim 非空格才产出；序 = 解析侧网格 r 优先序）。
struct CellSlot {
    /// 与解析侧同口径的格文本（格内文本拍平拼接后 trim）。
    text: String,
    /// XML 元素与格一一对应（`table:number-columns-repeated` 缺省/为 1）
    /// 才可回写；重复格占单元位但不回写。
    editable: bool,
    /// 格内容区 `[inner_start, inner_end)`（TranslationOnly 替换区）。
    inner: (usize, usize),
    /// 格内最后一个 `text:p` / `text:h` 的 End 后偏移（BilingualInline 插位）。
    last_para_after: Option<usize>,
    /// 上述段落的 `text:style-name` 属性值（原样字节；无则 None）。
    last_para_style: Option<String>,
}

/// 重扫描结果：段落槽位序列（item 序）、表格格位序列（表序）+
/// `</office:text>` 起始偏移。
struct Scanned {
    slots: Vec<OdtSlot>,
    tables: Vec<Vec<CellSlot>>,
    text_close: usize,
}

/// quick-xml 0.42 的 `buffer_position()` 是 `u64`；本模块 span 统一 `usize`。
fn bpos<R: std::io::BufRead>(reader: &Reader<R>) -> usize {
    reader.buffer_position() as usize
}

/// 扫描 `office:text`：与 [`crate::backends::odf::odt::odt_raw_page`] 完全
/// 同款的遍历（h/p 逐元素消费、table 格级扫描、其余容器深度递归），附带
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
    let mut tables: Vec<Vec<CellSlot>> = Vec::new();
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
                    tags::TABLE => {
                        // 格级扫描（table_rows 镜像）；0 行的表解析侧不产
                        // Table 块，这里同样不收（保持表序对齐）
                        let (cells, rows_pushed) = consume_table(&mut reader)?;
                        if rows_pushed > 0 {
                            tables.push(cells);
                        }
                    }
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
    Ok(Scanned {
        slots,
        tables,
        text_close,
    })
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

/// 消费一个 `table:table` 子树：与 [`crate::backends::odf::table_rows`]
/// 完全同款的行列遍历（每个 `table:table-row` 元素一行——
/// `table:number-rows-repeated` 解析侧不展开，镜像；逐 `table:table-cell` /
/// `table:covered-table-cell` 按 `table:number-columns-repeated` 展开占列，
/// `number-columns-spanned` 解析侧不读故忽略），附带格级字节锚点。返回
/// 非空格的格位序列（r 优先序 = 解析侧网格遍历序 = 单元流序）与已收行数
/// （0 = 解析侧不产 Table 块，调用方同样不收）。
fn consume_table<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<(Vec<CellSlot>, usize)> {
    let mut cells: Vec<CellSlot> = Vec::new();
    let mut rows_pushed = 0usize;
    let mut in_row = false;
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().local_name();
                match name.as_ref() {
                    tags::TABLE_CELL | tags::COVERED_CELL => {
                        let repeat = repeat_of(e, tags::COLS_REPEATED);
                        let cell = consume_cell(reader, name.as_ref())?;
                        if in_row && !cell.text.is_empty() {
                            // trim 非空才占单元位（空格不发单元，解析侧同口径）
                            if repeat == 1 {
                                cells.push(cell);
                            } else {
                                // 重复格：解析侧展开成多份相同格值、单元流发
                                // 多个单元——照常占满单元位保证后续格不错位，
                                // 但一个 XML 元素承载不了多份译文，一律不回写
                                let text = cell.text.clone();
                                cells.push(CellSlot {
                                    editable: false,
                                    ..cell
                                });
                                for _ in 1..repeat {
                                    cells.push(CellSlot {
                                        text: text.clone(),
                                        editable: false,
                                        inner: (0, 0),
                                        last_para_after: None,
                                        last_para_style: None,
                                    });
                                }
                            }
                        }
                    }
                    tags::TABLE_ROW => {
                        in_row = true;
                        depth += 1;
                    }
                    _ => depth += 1,
                }
            }
            // 自闭合格（`<table:table-cell/>`）与其它事件落到底部 `_ => {}`：
            // 解析侧按 repeat 份空串占列——空格不发单元，这里无需登记
            Ok(Event::End(ref e)) => {
                let name = e.name().local_name();
                if name.as_ref() == tags::TABLE_ROW && in_row {
                    rows_pushed += 1;
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
    Ok((cells, rows_pushed))
}

/// 消费一个 `table:table-cell` / `table:covered-table-cell` 元素：格文本与
/// 解析侧（`table_rows` → `text_until`）完全同口径——格内一切拍平拼接
/// （多段落直接相连、嵌套结构透明、显式空格/制表/换行转换），附带格内容
/// 区字节 span 与格内最后一个 `text:p` / `text:h` 的样式名 + End 后偏移
/// （BilingualInline 插位；任意深度的段落皆可——「text:p 之后插 text:p」
/// 对其父容器结构恒合法）。
fn consume_cell<R: std::io::BufRead>(reader: &mut Reader<R>, end: &str) -> Result<CellSlot> {
    let inner_start = bpos(reader);
    let mut out = String::new();
    let mut buf = Vec::new();
    let mut depth = 1usize;
    let (mut inner_end, mut last_after, mut last_style) = (inner_start, None, None);
    loop {
        let pos_before = bpos(reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().local_name();
                match name.as_ref() {
                    tags::S => out.push_str(&" ".repeat(repeat_of(e, tags::C))),
                    tags::TAB => out.push('\t'),
                    tags::LINE_BREAK => out.push('\n'),
                    tags::P | tags::H => {
                        // 格内段落：consume_text_elem 同口径吃掉（文本并入格
                        // 文本），并捕获样式与 End 后偏移作插位锚点
                        let style = attr(e, "style-name");
                        let (t, _inner, after) = consume_text_elem(reader, name.as_ref())?;
                        out.push_str(&t);
                        last_style = style;
                        last_after = Some(after);
                    }
                    _ => depth += 1,
                }
            }
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
                        inner_end = pos_before;
                        break;
                    }
                } else {
                    depth = depth.saturating_sub(1);
                }
            }
            Ok(Event::Text(ref e)) => {
                // 纯缩进空白节点丢弃（ODF 的显式空格走 text:s；解析侧同口径）
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
    Ok(CellSlot {
        text: out.trim().to_string(),
        editable: true,
        inner: (inner_start, inner_end),
        last_para_after: last_after,
        last_para_style: last_style,
    })
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
        let raw = OdtRawBackend.parse(&path).unwrap();
        OdfPlugin.assemble(&raw)
    }

    /// 提取侧全量单元流（wave2 起回写器消费全量流：段落类单元随段落块
    /// 就地配对，格单元随表格块就地配对）——helper 与该消费口径一致。
    fn unit_ids(path: &Path) -> Vec<String> {
        to_translation_units(&parse_doc(path))
            .into_iter()
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
    /// （译段沿用原段样式）、表格未提供格译文时不受扰、1 字符短段不占翻译
    /// 位、未匹配 id 忽略、其余条目透传、mimetype 仍居首。
    #[test]
    fn roundtrip_bilingual_inline() {
        let dir = case_dir("inline");
        let in_path = dir.join("in.odt");
        build_odt(&in_path, BODY);
        let out_path = dir.join("out.odt");

        // 回写消费 id 口径（乙 1 字符不发锚点；表格占 b5，其格单元
        // p0-b5-r0c0 随表格块配对——本测试不译它，表格不受扰）
        assert_eq!(
            unit_ids(&in_path),
            ["p0-b0", "p0-b1", "p0-b3", "p0-b4", "p0-b5-r0c0"]
        );

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

    /// 表格单元格级替换（wave2 金测）：TranslationOnly——格内容区整体替换为
    /// 单个译段，重解析后格文本 == 译文；部分命中格原样；多段落格的拍平
    /// 拼接口径（与解析侧 text_until 一致，多段直接相连）；格级编辑不产生
    /// 新块（格内 text:p 不进 item 流）。
    #[test]
    fn table_cells_translation_only_roundtrip() {
        let dir = case_dir("cells-tonly");
        let body = concat!(
            r#"<table:table table:name="T"><table:table-row>"#,
            r#"<table:table-cell><text:p>表头甲</text:p></table:table-cell>"#,
            r#"<table:table-cell><text:p>表头乙</text:p></table:table-cell>"#,
            r#"</table:table-row><table:table-row>"#,
            // 多段落格：解析侧拍平为「段一。段二。」（无分隔符直连）
            r#"<table:table-cell><text:p>段一。</text:p><text:p>段二。</text:p></table:table-cell>"#,
            r#"<table:table-cell><text:p>格乙二</text:p></table:table-cell>"#,
            r#"</table:table-row></table:table>"#,
        );
        let in_path = dir.join("in.odt");
        build_odt(&in_path, body);
        let out_path = dir.join("out.odt");

        // 表格占 b0；格单元按网格 r 优先序展开（多段格文本 = 拍平拼接）
        assert_eq!(
            unit_ids(&in_path),
            ["p0-b0-r0c0", "p0-b0-r0c1", "p0-b0-r1c0", "p0-b0-r1c1"]
        );

        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0-r0c0", "Header A"),
                ("p0-b0-r0c1", "Header B"),
                // r1c0（多段格）不翻译：部分命中，原样
                ("p0-b0-r1c1", "Cell B2"),
            ],
            BiStrategy::TranslationOnly,
        );

        let blocks = &parse_doc(&out_path).pages[0].blocks;
        assert_eq!(blocks.len(), 1, "格级编辑不产生新块");
        assert_eq!(blocks[0].kind, Table);
        assert_eq!(
            blocks[0].table.as_ref().unwrap().rows,
            vec![
                vec!["Header A".to_string(), "Header B".to_string()],
                vec!["段一。段二。".to_string(), "Cell B2".to_string()],
            ]
        );

        // 命中格的原文本不再出现；未命中格原样
        let content_xml = entry_str(&out_path, "content.xml");
        assert!(!content_xml.contains("表头甲"));
        assert!(!content_xml.contains("表头乙"));
        assert!(content_xml.contains("<text:p>段一。</text:p><text:p>段二。</text:p>"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 对齐考点（解析侧镜像规则）：`table:number-columns-spanned` 解析侧
    /// 不读——跨列格只占自己那格、covered 空格占下一列（列游标由格元素
    /// 逐一推进）；`table:number-columns-repeated` 按次数展开——重复格发
    /// 多个单元、照常占满单元位（保证后续格不错位），但一个 XML 元素承载
    /// 不了多份译文，重复格一律不回写（宁漏勿错）。
    #[test]
    fn table_cells_span_and_repeat_alignment() {
        let dir = case_dir("cells-align");
        let body = concat!(
            r#"<table:table table:name="T"><table:table-row>"#,
            r#"<table:table-cell table:number-columns-spanned="2"><text:p>跨列甲。</text:p></table:table-cell>"#,
            r#"<table:covered-table-cell/>"#,
            r#"</table:table-row><table:table-row>"#,
            r#"<table:table-cell table:number-columns-repeated="2"><text:p>重复格</text:p></table:table-cell>"#,
            r#"<table:table-cell><text:p>尾格。</text:p></table:table-cell>"#,
            r#"</table:table-row></table:table>"#,
        );
        let in_path = dir.join("in.odt");
        build_odt(&in_path, body);
        let out_path = dir.join("out.odt");

        // r0：跨列格 r0c0 + covered 空格（不发单元）；r1：重复格展开两份
        // （r1c0/r1c1 同文本）+ 尾格 r1c2——单元位照常占满、尾格 id 不错位
        assert_eq!(
            unit_ids(&in_path),
            ["p0-b0-r0c0", "p0-b0-r1c0", "p0-b0-r1c1", "p0-b0-r1c2"]
        );

        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0-r0c0", "Spanned A."),
                ("p0-b0-r1c0", "Rep A."),
                ("p0-b0-r1c1", "Rep B."),
                ("p0-b0-r1c2", "Last C."),
            ],
            BiStrategy::TranslationOnly,
        );

        let got = parse_doc(&out_path);
        let rows = &got.pages[0].blocks[0].table.as_ref().unwrap().rows;
        assert_eq!(
            rows,
            &vec![
                vec!["Spanned A.".to_string(), "".to_string()],
                vec![
                    "重复格".to_string(),
                    "重复格".to_string(),
                    "Last C.".to_string()
                ],
            ],
            "跨列格正常替换；重复格不回写（两份译文一并放弃）；尾格对齐不错位"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BilingualInline 格内口径：格内最后一个 `text:p` 之后追加译段（沿用
    /// 该段 `text:style-name`）；多段格的插位落在末段之后；格内译段不进
    /// item 流（块数不变）。
    #[test]
    fn table_cells_bilingual_inline_appends_para() {
        let dir = case_dir("cells-inline");
        let body = concat!(
            r#"<table:table table:name="T"><table:table-row>"#,
            r#"<table:table-cell><text:p text:style-name="Standard">格甲</text:p></table:table-cell>"#,
            r#"<table:table-cell><text:p>段一。</text:p><text:p>段二。</text:p></table:table-cell>"#,
            r#"</table:table-row></table:table>"#,
        );
        let in_path = dir.join("in.odt");
        build_odt(&in_path, body);
        let out_path = dir.join("out.odt");
        write(
            &in_path,
            &out_path,
            &[("p0-b0-r0c0", "Cell A"), ("p0-b0-r0c1", "Cell B")],
            BiStrategy::BilingualInline,
        );

        // 译段紧贴原段（样式沿用）；多段格插在末段之后
        let content_xml = entry_str(&out_path, "content.xml");
        assert!(content_xml.contains(
            r#"<text:p text:style-name="Standard">格甲</text:p><text:p text:style-name="Standard">Cell A</text:p>"#
        ));
        assert!(content_xml.contains("<text:p>段二。</text:p><text:p>Cell B</text:p>"));

        // 重解析：格文本 = 原文与译文拍平直连（解析侧无分隔符口径）；
        // 格内 text:p 不进 item 流
        let blocks = &parse_doc(&out_path).pages[0].blocks;
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].table.as_ref().unwrap().rows,
            vec![vec![
                "格甲Cell A".to_string(),
                "段一。段二。Cell B".to_string()
            ]]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：表格不进附录、原样——格译文无处安放，正文
    /// 表格逐字保留；附录只收段落类译文。
    #[test]
    fn table_cells_appendix_untouched() {
        let dir = case_dir("cells-appendix");
        let body = concat!(
            r#"<text:p>正文一段。</text:p>"#,
            r#"<table:table table:name="T"><table:table-row>"#,
            r#"<table:table-cell><text:p>格甲</text:p></table:table-cell>"#,
            r#"<table:table-cell><text:p>格乙</text:p></table:table-cell>"#,
            r#"</table:table-row></table:table>"#,
        );
        let in_path = dir.join("in.odt");
        build_odt(&in_path, body);
        let out_path = dir.join("out.odt");

        // 段落 b0 + 表格 b1（格单元 r0c0/r0c1）
        assert_eq!(unit_ids(&in_path), ["p0-b0", "p0-b1-r0c0", "p0-b1-r0c1"]);

        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "First paragraph."),
                ("p0-b1-r0c0", "Cell A"),
                ("p0-b1-r0c1", "Cell B"),
            ],
            BiStrategy::OriginalPlusAppendix,
        );

        let content_xml = entry_str(&out_path, "content.xml");
        assert!(content_xml.contains("<text:p>格甲</text:p>"), "正文表格原样");
        assert!(content_xml.contains("<text:p>格乙</text:p>"));
        assert!(
            !content_xml.contains("Cell A") && !content_xml.contains("Cell B"),
            "格译文不进附录"
        );

        // 附录只含段落译文（插入即紧贴 </office:text>）
        let close = content_xml.rfind("</office:text>").unwrap();
        let h = content_xml
            .find(r#"<text:h text:outline-level="1">译文</text:h>"#)
            .unwrap();
        assert_eq!(
            &content_xml[h..close],
            r#"<text:h text:outline-level="1">译文</text:h><text:p>First paragraph.</text:p>"#
        );

        // 重解析：表格保持原格
        let blocks = &parse_doc(&out_path).pages[0].blocks;
        let table = blocks
            .iter()
            .find(|b| b.kind == Table)
            .unwrap()
            .table
            .as_ref()
            .unwrap();
        assert_eq!(table.rows, vec![vec!["格甲".to_string(), "格乙".to_string()]]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
