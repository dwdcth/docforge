// docforge 新增（翻译闭环①写侧）：docx 双语回写。
//! DOCX 双语回写：解包原 ZIP → 重解析建立块序对齐 → 改写 `word/document.xml`
//! → 其余部件字节透传，重打包到 `out`。
//!
//! # 对齐口径（与 `backends/office/docx.rs` 解析侧逐条对应）
//!
//! * 单元流 ↔ XML 位序列一一对应：[`crate::emit::to_translation_units`]
//!   全量流（段落单元 + 表格格单元按文档序交错）配对本扫描产出的「可翻译
//!   位」序列：
//!   - 段落位：body 直属（及 sdt 内）trim 非空 + 字符数 ≥ 2 的 `w:p`
//!     （与单元流同口径）；自闭合 `<w:p/>` 无文本，不占位；
//!   - 格位：`w:tbl` 内 trim 非空的 `w:tc`（格阈值恒 1 = trim 非空即收，
//!     不受段落位 ≥2 字符约束）。网格口径与解析侧 `extract_table_rows`
//!     逐条镜像：**一格一列**——解析侧对 `gridSpan`/`vMerge` 不做展开，
//!     `TableData.rows` 的行 = `w:tr` 文档序、列 = `w:tc` 在其 `w:tr` 内的
//!     文档序，空格在网格占列但不发单元；格文本 = 格内直接子级 `w:p` 的
//!     非空段（trim）join `\n`（嵌套表按解析侧口径展平同位并入）。行/列
//!     坐标由提取侧锚点（`-r{行}c{列}`）承载，回写只按文档序配对，不做
//!     坐标换算；
//!   - `w:tbl` 子树内的 `w:p` 属于格（或嵌套表），不再是正文段落位——与
//!     解析侧一致。
//! * 文本提取语义与解析侧 `extract_para` 相同：只收 `w:t` 文本（含实体解码
//!   / CDATA），`w:tab`→`\t`、`w:br`/`w:cr`→`\n`、`w:noBreakHyphen`→`-`；
//!   文本框等嵌套 `w:p` 的 `w:t` 并入外层段落文本（解析侧同此行为）。
//! * 双保险：每个位与重解析的对应单元做文本一致性比对（格位同款），不一致
//!   跳过该位不写、单元照常消费保流对齐——宁漏勿错（防解析器演进后错位写）。
//! * 三策略的表格口径：`TranslationOnly` 格内首 `w:t` 换译文、其余清空
//!   （多段落格的其余段一并清空——重解析格文本 = 译文，与提取侧 join 口径
//!   一致）；`BilingualInline` 格内最后一个 `w:p` 之后追加译段（复制格内
//!   首段 `w:pPr`；不用 `w:br` 软换行——会破 docx 表格行高语义）；
//!   `OriginalPlusAppendix` 表格不进附录（附录只收段落/标题位），格位照常
//!   配对消费保流对齐、正文表格原样不动。
//! * v1 简化（写入侧，文本守卫兜底）：含嵌套表的格不写（清空/追加会破坏
//!   嵌套表文本或留 `,` 展平残留），格位照常占位；格内富文本 run 结构只保
//!   留不清（同段落的 TranslationOnly 简化口径）。
//! * `w:sdt`（内容控件，正文里多为 TOC）：取其子树内的 `w:p` 作候选位，
//!   sdt 内表格仍整棵跳过。已知差异：解析侧 `extract_sdt_items` 的深度计数
//!   在提取段落后不回落，sdt 之后的表格会被吞进 sdt 循环、其段落被当正文
//!   段落提取（上游特性）；本扫描按良构深度跟踪——若文档真踩到此差异，
//!   文本一致性守卫会让受影响位保守跳过而不是错位写。

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
/// [`crate::emit::to_translation_units`] 同口径（块 `p{页}-b{块序}`、表格格
/// `p{页}-b{块序}-r{行}c{列}`）；未匹配 / 空白译文条目忽略；失败返回 `corrupt`。
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
    // 单元流取全量：段落单元 + 表格格单元按文档序交错（格单元由 XML 扫描的
    // w:tbl 格位配对，见模块头对齐口径）。
    let raw = DocxRawBackend::default().parse(path_str)?;
    let doc = OfficePlugin::default().assemble(&raw);
    let units = crate::emit::to_translation_units(&doc);
    let map = translation_map(translated);

    // 第二遍：XML 扫描可翻译位（段落位 + 格位），与单元按序对齐。
    let mut zip = open_zip(in_path)?;
    let xml = crate::backends::office::zip_read_str(&mut zip, "word/document.xml")?
        .ok_or_else(|| DocError::corrupt("missing word/document.xml"))?;
    let (slots, body_close) = scan_slots(&xml)?;

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
// 可翻译位扫描（quick-xml 流式，带字节 span；段落位 + 表格格位）
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

/// 一个「表格格位」：`<w:tc>` 的格文本与格内直接子级 `w:p` 段位。
#[derive(Debug, Clone)]
struct CellSlot {
    /// 格文本——解析侧 `extract_table_rows` 单格累积同口径：直接子级 `w:p`
    /// 的非空段（trim）按文档序 join `\n`，嵌套表展平串同位并入；整体 trim。
    text: String,
    /// 格内直接子级 `w:p` 段位（文档序；嵌套表内的段落不在其中）。
    paras: Vec<ParaSlot>,
    /// 格内含嵌套 `w:tbl`。v1 不写此类格（见模块头），格位照常占位保对齐。
    has_nested_table: bool,
}

impl CellSlot {
    /// 格的合格判定：trim 非空即占位（格阈值恒 1，与提取侧单元格展开口径
    /// 一致，不受段落位的 ≥2 字符约束）。
    fn is_candidate(&self) -> bool {
        !self.text.trim().is_empty()
    }
}

/// 一个「可翻译位」：正文段落位或表格格位，按文档序与单元流一一配对。
#[derive(Debug, Clone)]
enum Slot {
    Para(ParaSlot),
    Cell(CellSlot),
}

impl Slot {
    /// 位文本（文本一致性守卫用；格位 = 格文本，段落位 = 段落文本）。
    fn text(&self) -> &str {
        match self {
            Slot::Para(p) => &p.text,
            Slot::Cell(c) => &c.text,
        }
    }
}

/// quick-xml 0.42 的 `buffer_position()` 是 `u64`；本模块 span 统一 `usize`。
fn bpos<R: std::io::BufRead>(reader: &Reader<R>) -> usize {
    reader.buffer_position() as usize
}

/// 扫描 body 直属子级，产出文档序的可翻译位（段落位 + 格位）与
/// `</w:body>` 起始偏移。`w:tbl` 产格位（见 [`consume_table`]）；
/// `w:sdt` 取子树内段落（sdt 内表格整棵跳过，见模块头已知差异）。
fn scan_slots(xml: &str) -> Result<(Vec<Slot>, usize)> {
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

    let mut slots: Vec<Slot> = Vec::new();
    let body_close = loop {
        let pos_before = bpos(&reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name().local_name().as_ref() {
                tags::W_P => {
                    let slot = consume_para(&mut reader, xml)?;
                    if slot.is_candidate() {
                        slots.push(Slot::Para(slot));
                    }
                }
                tags::W_TBL => consume_table(&mut reader, xml, &mut slots)?,
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
    slots: &mut Vec<Slot>,
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
                            slots.push(Slot::Para(slot));
                        }
                        // consume_para 吃掉了配对 End，深度不增减（对齐解析侧）
                        continue;
                    }
                    tags::W_TBL => {
                        // sdt 内表格：整棵跳过（解析侧 quirk 使其段落被当正文
                        // 段落提取，见模块头已知差异；文本一致性守卫兜底）
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
// 表格格位扫描（与解析侧 extract_table_rows 网格口径逐条镜像）
// ---------------------------------------------------------------------------

/// 消费 body 直属 `<w:tbl>` 子树（调用时 reader 刚读完 `<w:tbl>` Start），
/// 把 trim 非空的 `w:tc` 登记为格位（文档序）。
///
/// 网格口径与解析侧完全同款（**一格一列**）：解析侧对 `gridSpan`/`vMerge`
/// 不做任何展开，`TableData.rows` 的行 = `w:tr` 文档序、列 = `w:tc` 在其
/// `w:tr` 内的文档序，空格（无文本 `w:tc`）在网格占列但不发单元。因此格位
/// 按 `w:tc` 文档序产出即与提取侧格单元（`-r{行}c{列}`，列 = 行内下标）
/// 一一对齐，无需网格坐标换算。嵌套表整棵由 [`consume_cell`] 消费（其段落
/// 不产位——解析侧把嵌套表展平进所在格）。
fn consume_table<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    xml: &str,
    slots: &mut Vec<Slot>,
) -> Result<()> {
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name().local_name().as_ref() {
                // w:tc 只在本表的 w:tr 直下出现（嵌套表的子树由 consume_cell
                // 整棵消费，不经过本循环）；consume_cell 吃掉配对 End，深度不增减
                tags::W_TC => {
                    let cell = consume_cell(reader, xml)?;
                    if cell.is_candidate() {
                        slots.push(Slot::Cell(cell));
                    }
                }
                _ => depth += 1,
            },
            Ok(Event::End(ref _e)) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            Ok(Event::Eof) => return Ok(()),
            Err(e) => return Err(DocError::corrupt(format!("XML table scan error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
}

/// 消费一个 `<w:tc>` 子树（调用时 reader 刚读完 `<w:tc>` Start），返回格位。
/// 格文本与解析侧单格累积逐条同款：直接子级 `w:p` 的非空段（trim）按文档序
/// join `\n`；嵌套 `w:tbl` 按解析侧口径展平、同位并入（并入判定用行数而非
/// 展平串是否为空）；末尾整体 trim（解析侧 `w:tc` End 同此）。
fn consume_cell<R: std::io::BufRead>(reader: &mut Reader<R>, xml: &str) -> Result<CellSlot> {
    let mut buf = Vec::new();
    let mut text = String::new();
    let mut paras: Vec<ParaSlot> = Vec::new();
    let mut has_nested_table = false;
    let mut has_content = false;
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name().local_name().as_ref() {
                tags::W_P => {
                    let para = consume_para(reader, xml)?;
                    // 与解析侧一致：非空段才进格文本；段位照常收（写入用）
                    let t = para.text.trim();
                    if !t.is_empty() {
                        if has_content {
                            text.push('\n');
                        }
                        text.push_str(t);
                        has_content = true;
                    }
                    paras.push(para);
                    // consume_para 吃掉了配对 End，深度不增减
                }
                tags::W_TBL => {
                    let (rows, flat) = consume_nested_table_text(reader, xml)?;
                    has_nested_table = true;
                    if rows > 0 {
                        if has_content {
                            text.push('\n');
                        }
                        text.push_str(&flat);
                        has_content = true;
                    }
                    // 嵌套表整棵已消费，深度不增减
                }
                _ => depth += 1,
            },
            Ok(Event::End(ref _e)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML cell scan error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(CellSlot {
        text: text.trim().to_string(),
        paras,
        has_nested_table,
    })
}

/// 消费嵌套 `<w:tbl>` 子树（调用时 reader 刚读完 `<w:tbl>` Start），按解析侧
/// `extract_table_rows` 的文本口径展平：格内非空段（trim）join `\n`、行内格
/// join `,`、行间 join `\n`。返回 (行数, 展平文本)——行数镜像解析侧
/// `!nested.is_empty()` 的并入判定（全空格行表也置 has_content，保证格文本
/// /候选判定与解析侧逐字节一致）。
fn consume_nested_table_text<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    xml: &str,
) -> Result<(usize, String)> {
    let mut buf = Vec::new();
    let mut depth = 1usize;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut cell_has_content = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name().local_name().as_ref() {
                tags::W_P => {
                    let para = consume_para(reader, xml)?;
                    let t = para.text.trim();
                    if !t.is_empty() {
                        if cell_has_content {
                            cell.push('\n');
                        }
                        cell.push_str(t);
                        cell_has_content = true;
                    }
                }
                tags::W_TBL => {
                    // 更深嵌套：递归展平（内部消费了配对 End，深度不增减）
                    let (nested_rows, nested_flat) = consume_nested_table_text(reader, xml)?;
                    if nested_rows > 0 {
                        if cell_has_content {
                            cell.push('\n');
                        }
                        cell.push_str(&nested_flat);
                        cell_has_content = true;
                    }
                }
                _ => depth += 1,
            },
            Ok(Event::End(ref e)) => {
                depth -= 1;
                match e.name().local_name().as_ref() {
                    tags::W_TBL if depth == 0 => {
                        if !row.is_empty() {
                            rows.push(std::mem::take(&mut row));
                        }
                        break;
                    }
                    tags::W_TR => {
                        // 行结束：单元格已由 W_TC End 处理，只需保存行
                        if !row.is_empty() {
                            rows.push(std::mem::take(&mut row));
                        }
                    }
                    tags::W_TC => {
                        // 单元格结束：保存内容（空格也占列，与解析侧一致）
                        row.push(cell.trim().to_string());
                        cell.clear();
                        cell_has_content = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML nested table scan error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    let flat = rows
        .iter()
        .map(|r| r.join(","))
        .collect::<Vec<_>>()
        .join("\n");
    Ok((rows.len(), flat))
}

// ---------------------------------------------------------------------------
// 三种策略
// ---------------------------------------------------------------------------

/// 位 × 翻译单元按序配对（文本一致性守卫）+ 译文查找。
/// 返回 (slot 下标, 译文) 列表，文档序；段落位与格位都在其中——
/// OriginalPlusAppendix 由调用方过滤只留段落位（表格不进附录）。
fn matched_translations(
    slots: &[Slot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, &str>,
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let n = slots.len().min(units.len());
    for k in 0..n {
        // 双保险：位文本与单元文本必须一致（同一解析器同一文件的确定性保证）
        if slots[k].text().trim() != units[k].text.trim() {
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

/// BilingualInline：在原 `</w:p>` 后插入译文段（复制原段 pPr，文本 = 译文）；
/// 格位则追在格内最后一个 `w:p` 之后（复制格内首段的 pPr；含嵌套表的格
/// v1 不写，见模块头）。
fn apply_inline(
    xml: &str,
    slots: &[Slot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, &str>,
) -> String {
    let matched = matched_translations(slots, units, map);
    if matched.is_empty() {
        return xml.to_string();
    }
    let mut edits: Vec<(usize, String)> = Vec::new();
    for (k, tr) in matched {
        match &slots[k] {
            Slot::Para(p) => {
                let ppr = p.ppr.clone().unwrap_or_default();
                edits.push((
                    p.end,
                    format!(
                        "<w:p>{ppr}<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                        escape_xml_text(&tr)
                    ),
                ));
            }
            Slot::Cell(c) => {
                // 插在最后一个 w:p 之后 = 格底追加（单段落格与「原位之后」
                // 等价）；pPr 取首段的（格内主段样式）。译文独立成段而非
                // w:br 软换行——软换行会破坏 docx 表格行高语义。
                if c.has_nested_table {
                    continue;
                }
                let Some(last) = c.paras.last() else {
                    continue;
                };
                let ppr = c.paras[0].ppr.clone().unwrap_or_default();
                edits.push((
                    last.end,
                    format!(
                        "<w:p>{ppr}<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                        escape_xml_text(&tr)
                    ),
                ));
            }
        }
    }
    splice(xml, edits)
}

/// TranslationOnly：首 `w:t` 文本换译文，其余 `w:t` 文本清空。格位口径：
/// 首个 `w:t` 取译文之外，格内全部直接段落的 `w:t` 一并清空（重解析格文本
/// = 译文，与提取侧格内多段落 join 口径一致）；含嵌套表的格 v1 不写——
/// 清空嵌套表文本会在重解析留下 `,` 展平残留。
/// 简化说明：不删 run 结构（保留书签/字段/格式），只清文本——重解析口径
/// （位文本 = 译文）与「首 run 换文本其余 run 删」等价，但更不易损坏文档。
fn apply_translation_only(
    xml: &str,
    slots: &[Slot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, &str>,
) -> String {
    let matched = matched_translations(slots, units, map);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for (k, tr) in matched {
        match &slots[k] {
            Slot::Para(p) => {
                let spans = &p.text_spans;
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
            Slot::Cell(c) => {
                if c.has_nested_table {
                    continue;
                }
                let mut it = c.paras.iter().flat_map(|p| p.text_spans.iter());
                let Some(first) = it.next() else {
                    continue; // 全格无文本节点（文本全来自 tab/br 占位）：保守跳过
                };
                edits.push((first.0, first.1, escape_xml_text(&tr)));
                for span in it {
                    edits.push((span.0, span.1, String::new()));
                }
            }
        }
    }
    splice_spans(xml, edits)
}

/// OriginalPlusAppendix：正文不动，body 末尾追加「译文」标题段 + 译文段序列。
/// 表格不进附录：格位照常参与配对（消费单元流保对齐），但不产出附录条目，
/// 格文本原样不动。
fn apply_appendix(
    xml: &str,
    slots: &[Slot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, &str>,
    body_close: usize,
) -> String {
    let matched: Vec<(usize, String)> = matched_translations(slots, units, map)
        .into_iter()
        .filter(|(k, _)| matches!(slots[*k], Slot::Para(_)))
        .collect();
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

    /// 回写消费的单元流 id（wave2 起全量：段落单元 + 表格格单元按文档序
    /// 交错，与 XML 扫描的段落位 + 格位一一配对）。
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

        // 回写消费单元流全量：短段占 b2 不进翻译流（1 字符 < min_chars）；
        // 表格占 b3，格单元 p0-b3-r0c0/r0c1 在流中但未给译文 → 格原样
        assert_eq!(
            unit_ids(&in_path),
            ["p0-b0", "p0-b1", "p0-b3-r0c0", "p0-b3-r0c1", "p0-b4"]
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

    /// sdt（内容控件）里的段落也是候选位；表格产格单元（未给译文 → 原样）。
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

        // 表格产 Table 块并按格展开（b0 → 格单元 r0c0）；sdt 段落是 b1
        assert_eq!(unit_ids(&in_path), ["p0-b0-r0c0", "p0-b1"]);

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

    // -------------------------------------------------------------------------
    // 表格单元格级替换（wave2）金测
    // -------------------------------------------------------------------------

    /// 两行表格 + 前后段落的文档：tblPr 边框 / tcPr 列宽属性字节用于断言
    /// 表格结构原样。
    const TABLE_DOC_XML: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:p><w:r><w:t>表格前的段落。</w:t></w:r></w:p>"#,
        r#"<w:tbl><w:tblPr><w:tblBorders><w:top w:val="single"/></w:tblBorders></w:tblPr>"#,
        r#"<w:tr><w:tc><w:tcPr><w:tcW w:w="4500" w:type="dxa"/></w:tcPr><w:p><w:r><w:t>名称</w:t></w:r></w:p></w:tc>"#,
        r#"<w:tc><w:p><w:r><w:t>数值</w:t></w:r></w:p></w:tc></w:tr>"#,
        r#"<w:tr><w:tc><w:p><w:r><w:t>电压</w:t></w:r></w:p></w:tc>"#,
        r#"<w:tc><w:p><w:r><w:t>220</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        r#"<w:p><w:r><w:t>表格后的段落。</w:t></w:r></w:p>"#,
        r#"</w:body></w:document>"#
    );

    /// 金测（表格核心验收）：TranslationOnly 单元格级替换往返 —— 单元流全量
    /// 提取 → 逐 id 给译文 → 回写 → 重解析：格文本 == 译文、非表格段落同样
    /// 换文、未命中的格原样、tblPr/tcPr 结构字节原样、styles.xml 透传。
    #[test]
    fn translation_only_replaces_table_cells() {
        let dir = case_dir("tbl_tonly");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, TABLE_DOC_XML);

        // 单元流全量：段落单元 + 格单元按文档序交错（格阈值恒 1，"220" 也是单元）
        let units = to_translation_units(&parse_doc(&in_path));
        assert_eq!(
            units.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            [
                "p0-b0",
                "p0-b1-r0c0",
                "p0-b1-r0c1",
                "p0-b1-r1c0",
                "p0-b1-r1c1",
                "p0-b2",
            ]
        );
        // 除 r1c1 外全部给译文（部分命中：未命中的格原样）
        let translated: Vec<(String, String)> = units
            .iter()
            .filter(|u| u.id != "p0-b1-r1c1")
            .map(|u| (u.id.clone(), format!("[{}]", u.text)))
            .collect();
        writeback_docx(&in_path, &translated, BiStrategy::TranslationOnly, &out_path).unwrap();

        let got = blocks(&out_path);
        assert_eq!(got.len(), 3, "TranslationOnly 无插入：块数与原文一致");
        assert_eq!(got[0].content, "[表格前的段落。]");
        let table = got[1].table.as_ref().unwrap();
        assert_eq!(
            table.rows,
            vec![
                vec!["[名称]".to_string(), "[数值]".to_string()],
                vec!["[电压]".to_string(), "220".to_string()], // r1c1 未给译文：原样
            ]
        );
        assert_eq!(got[2].content, "[表格后的段落。]");

        // 表格结构（边框/列宽属性字节）原样 + 其余部件透传
        let out_xml = entry_str(&out_path, "word/document.xml");
        assert!(out_xml.contains(r#"<w:tblBorders><w:top w:val="single"/></w:tblBorders>"#));
        assert!(out_xml.contains(r#"<w:tcW w:w="4500" w:type="dxa"/>"#));
        let mut zin = open_zip(&in_path).unwrap();
        let mut zout = open_zip(&out_path).unwrap();
        let mut a = String::new();
        let mut b = String::new();
        zin.by_name("word/styles.xml").unwrap().read_to_string(&mut a).unwrap();
        zout.by_name("word/styles.xml").unwrap().read_to_string(&mut b).unwrap();
        assert_eq!(a, b);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// gridSpan / 空格对齐：解析侧一格一列（列 = `w:tc` 文档序，无网格坐标
    /// 换算），跨列格列 = 0、被跨的网格列在 XML 无 `w:tc` 无位不写；空格占
    /// 列位但不发单元（其后格的列号跳位）。
    #[test]
    fn gridspan_and_empty_cells_align_by_document_order() {
        let dir = case_dir("tbl_span");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        const SPAN_XML: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:tbl>"#,
            r#"<w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>跨列标题</w:t></w:r></w:p></w:tc></w:tr>"#,
            r#"<w:tr><w:tc><w:tcPr><w:gridSpan w:val="3"/></w:tcPr><w:p><w:r><w:t>甲</w:t></w:r></w:p></w:tc>"#,
            r#"<w:tc><w:p><w:r><w:t>乙</w:t></w:r></w:p></w:tc></w:tr>"#,
            r#"<w:tr><w:tc><w:p><w:r><w:t>丙</w:t></w:r></w:p></w:tc>"#,
            r#"<w:tc><w:p><w:r><w:t xml:space="preserve"> </w:t></w:r></w:p></w:tc>"#,
            r#"<w:tc><w:p><w:r><w:t>丁</w:t></w:r></w:p></w:tc></w:tr>"#,
            r#"</w:tbl></w:body></w:document>"#
        );
        build_docx(&in_path, SPAN_XML);

        // 格阈值恒 1：单字符格（甲/乙/丙/丁）都进流；空格占 r2 列 1 但不发
        // 单元 → 丁的列 = 2；跨列格只是 (r,0)（无 gridSpan 展开）
        assert_eq!(
            unit_ids(&in_path),
            [
                "p0-b0-r0c0",
                "p0-b0-r1c0",
                "p0-b0-r1c1",
                "p0-b0-r2c0",
                "p0-b0-r2c2",
            ]
        );

        let units = to_translation_units(&parse_doc(&in_path));
        let translated: Vec<(String, String)> = units
            .iter()
            .map(|u| (u.id.clone(), format!("译·{}", u.text)))
            .collect();
        writeback_docx(&in_path, &translated, BiStrategy::TranslationOnly, &out_path).unwrap();

        let out_blocks = blocks(&out_path);
        let table = out_blocks[0].table.as_ref().unwrap();
        assert_eq!(
            table.rows,
            vec![
                vec!["译·跨列标题".to_string()],
                vec!["译·甲".to_string(), "译·乙".to_string()],
                vec!["译·丙".to_string(), String::new(), "译·丁".to_string()],
            ]
        );
        // gridSpan 属性字节原样（结构不受写入扰动）
        let out_xml = entry_str(&out_path, "word/document.xml");
        assert!(out_xml.contains(r#"<w:gridSpan w:val="2"/>"#));
        assert!(out_xml.contains(r#"<w:gridSpan w:val="3"/>"#));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v1 简化口径钉死：含嵌套表的格——格位照常占位（格文本 = 直接段 join
    /// 嵌套表展平串，与解析侧逐字节同口径 → 文本守卫通过）、不写入（原样），
    /// 其余格照常替换；单元流全程对齐。
    #[test]
    fn nested_table_cell_occupies_slot_but_stays_unwritten() {
        let dir = case_dir("tbl_nested");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        const NESTED_XML: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:tbl><w:tr>"#,
            r#"<w:tc><w:p><w:r><w:t>外格</w:t></w:r></w:p>"#,
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>内甲</w:t></w:r></w:p></w:tc>"#,
            r#"<w:tc><w:p><w:r><w:t>内乙</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
            r#"<w:p><w:r><w:t>尾段</w:t></w:r></w:p></w:tc>"#,
            r#"<w:tc><w:p><w:r><w:t>普通格</w:t></w:r></w:p></w:tc>"#,
            r#"</w:tr></w:tbl>"#,
            r#"</w:body></w:document>"#
        );
        build_docx(&in_path, NESTED_XML);

        // 格文本 = 直接段（外格/尾段）join 嵌套表展平串（行内 `,`）——解析侧同口径
        let units = to_translation_units(&parse_doc(&in_path));
        assert_eq!(
            units
                .iter()
                .map(|u| (u.id.as_str(), u.text.as_str()))
                .collect::<Vec<_>>(),
            [
                ("p0-b0-r0c0", "外格\n内甲,内乙\n尾段"),
                ("p0-b0-r0c1", "普通格"),
            ]
        );

        // 两个格位都在流上且文本守卫通过（嵌套展平镜像与解析侧一致 → 不因
        // 守卫漏配；配对是位置的，守卫只把关写不写）
        let translated = vec![
            ("p0-b0-r0c0".to_string(), "[外格]".to_string()),
            ("p0-b0-r0c1".to_string(), "[普通格]".to_string()),
        ];
        let xml = entry_str(&in_path, "word/document.xml");
        let (slots, _) = scan_slots(&xml).unwrap();
        let entries = translated.clone();
        let matched = matched_translations(&slots, &units, &translation_map(&entries));
        assert_eq!(
            matched.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            [0, 1],
            "两个格位都命中（流对齐 + 展平镜像精确）"
        );

        // TranslationOnly：嵌套格不写（原样），普通格替换
        writeback_docx(&in_path, &translated, BiStrategy::TranslationOnly, &out_path).unwrap();
        let out_blocks = blocks(&out_path);
        let table = out_blocks[0].table.as_ref().unwrap();
        assert_eq!(
            table.rows,
            vec![vec![
                "外格\n内甲,内乙\n尾段".to_string(),
                "[普通格]".to_string(),
            ]]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BilingualInline：格内最后一段之后追加译段（复制格内首段 pPr，独成
    /// `w:p` 而非 `w:br` 软换行）；多段落格重解析 = 原文 join 译文（提取侧
    /// 格内多段落 join 同口径）。
    #[test]
    fn bilingual_inline_appends_translation_paragraph_in_cell() {
        let dir = case_dir("tbl_inline");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        const CELL_INLINE_XML: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:tbl><w:tr>"#,
            r#"<w:tc><w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>甲段一</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>甲段二</w:t></w:r></w:p></w:tc>"#,
            r#"<w:tc><w:p><w:r><w:t>乙</w:t></w:r></w:p></w:tc>"#,
            r#"</w:tr></w:tbl>"#,
            r#"</w:body></w:document>"#
        );
        build_docx(&in_path, CELL_INLINE_XML);

        // 格文本 = 格内多段落 join（提取侧同口径）
        let units = to_translation_units(&parse_doc(&in_path));
        assert_eq!(
            units
                .iter()
                .map(|u| (u.id.as_str(), u.text.as_str()))
                .collect::<Vec<_>>(),
            [("p0-b0-r0c0", "甲段一\n甲段二"), ("p0-b0-r0c1", "乙")]
        );

        let translated = vec![
            ("p0-b0-r0c0".to_string(), "Cell translation".to_string()),
            ("p0-b0-r0c1".to_string(), "Beta".to_string()),
        ];
        writeback_docx(&in_path, &translated, BiStrategy::BilingualInline, &out_path).unwrap();

        let out_blocks = blocks(&out_path);
        let table = out_blocks[0].table.as_ref().unwrap();
        assert_eq!(
            table.rows,
            vec![vec![
                "甲段一\n甲段二\nCell translation".to_string(),
                "乙\nBeta".to_string(),
            ]]
        );
        // 追加段复制格内首段的 pPr（Heading2），独成 w:p
        let out_xml = entry_str(&out_path, "word/document.xml");
        assert!(out_xml.contains(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t xml:space="preserve">Cell translation</w:t></w:r></w:p>"#
        ));
        assert_eq!(
            out_xml.matches("<w:p>").count(),
            5,
            "格1 两原段 + 译段、格2 原段 + 译段"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：表格不进附录（格位照常配对消费，条目被过滤），
    /// 正文格文本原样；附录只收段落译文。
    #[test]
    fn appendix_excludes_table_cells_table_kept_intact() {
        let dir = case_dir("tbl_appendix");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, TABLE_DOC_XML);

        // 全部单元（含格）都给译文：附录仍只收段落
        let units = to_translation_units(&parse_doc(&in_path));
        let translated: Vec<(String, String)> = units
            .iter()
            .map(|u| (u.id.clone(), format!("[{}]", u.text)))
            .collect();
        writeback_docx(&in_path, &translated, BiStrategy::OriginalPlusAppendix, &out_path).unwrap();

        let orig = blocks(&in_path);
        let got = blocks(&out_path);
        assert_eq!(
            got.len(),
            orig.len() + 3,
            "追加标题 1 + 段落译文 2（格单元不进附录）"
        );
        assert_eq!(got[..orig.len()], orig[..], "正文逐块不动（表格格文本原样）");
        let appendix: Vec<&str> = got[orig.len() + 1..]
            .iter()
            .map(|b| b.content.as_str())
            .collect();
        assert_eq!(appendix, ["[表格前的段落。]", "[表格后的段落。]"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文本守卫（格位同款）：位文本 ≠ 单元文本 → 不写（宁漏勿错）；
    /// 位置配对是权威，文本守卫只把关写不写。
    #[test]
    fn cell_text_guard_skips_mismatched_slot() {
        let unit = |id: &str, text: &str, kind: BlockKind| crate::emit::TranslationUnit {
            id: id.to_string(),
            page: Some("0".to_string()),
            kind,
            text: text.to_string(),
            source: crate::model::BlockSource::Native,
            confidence: None,
            lang: None,
            chapter: None,
            header: false,
        };
        let entries = [
            ("p0-b0-r0c0".to_string(), "格译文".to_string()),
            ("p0-b1".to_string(), "段译文".to_string()),
        ];
        let map = translation_map(&entries);

        // 格文本 / 段落文本都与对应单元不一致 → 全部不命中
        let slots = [
            Slot::Cell(CellSlot {
                text: "格文本".to_string(),
                paras: Vec::new(),
                has_nested_table: false,
            }),
            Slot::Para(ParaSlot {
                end: 0,
                text: "段落文本".to_string(),
                ppr: None,
                text_spans: Vec::new(),
            }),
        ];
        let units = vec![
            unit("p0-b0-r0c0", "单元文本甲", Table),
            unit("p0-b1", "单元文本乙", Paragraph),
        ];
        assert!(
            matched_translations(&slots, &units, &map).is_empty(),
            "文本不一致的位不写"
        );

        // 反例：文本一致时照常命中（守卫只把关写不写，不破坏流配对）
        let ok_units = vec![
            unit("p0-b0-r0c0", "格文本", Table),
            unit("p0-b1", "段落文本", Paragraph),
        ];
        assert_eq!(
            matched_translations(&slots, &ok_units, &map)
                .iter()
                .map(|(k, tr)| (*k, tr.as_str()))
                .collect::<Vec<_>>(),
            [(0, "格译文"), (1, "段译文")]
        );
    }
}
