// docforge 新增（翻译闭环①写侧）：HTML 单文件双语回写 + XHTML 扫描共用件。
//! HTML 单文件双语回写：整篇 `parse_blocks` 同口径重块化对齐 + 文本节点级改写。
//!
//! # 对齐口径（与 `plugins/html.rs` 提取侧逐条对应）
//!
//! * 提取侧管线：`WholeTextParseCapability`（"html.parse"，整篇单 item）→
//!   `HtmlPlugin::assemble`（`parse_blocks` 块化，单页）。回写侧**重跑同一管线**
//!   拿权威 id/文本（`p0-b{块序}`，页名 None → 页序 0），再以 `scan_html_blocks`
//!   （与 `parse_blocks` 完全同款的触发顺序 / 空白跳过 / 实体解码，附带字节
//!   span）把每个块映回「块 × 文本节点 span」。块序 = 解析序，稳定。
//! * 可翻译判据 = [`crate::emit::is_translatable`] + `min_chars = 2` + 空白
//!   跳过（与 `to_translation_units` 同口径）；Table 块按单元格展开进翻译流
//!   （格单元 `kind = Table`，回写在表格位逐格消费，见下「表格格级回写」），
//!   Image/Code 结构块不进翻译流但仍占块序号（锚点不重排）。
//! * 双保险：每个定位到的块与重块化文本做一致性比对，不一致跳过该块
//!   （宁漏勿错）；未匹配 id / 空白译文一律忽略。
//! * 编码：仅支持 UTF-8 件（含 BOM，剥除后扫描、写回时补回）——非 UTF-8
//!   件回写会与 `<meta charset>` 声明失配，直接拒绝（宁漏勿错）。
//!
//! # 共用件
//!
//! 本文件同时承载 epub 回写（`writeback/epub.rs`）复用的 XHTML 扫描器：
//! `scan_html_blocks` / `HtmlSlot` / `HtmlCellSlot` 与
//! `plugins/html.rs::parse_blocks` 逐行同口径（epub 逐 spine 文档调用，
//! 本文件对单文件调用一次）。
//!
//! # 三种策略（[`crate::emit::BiStrategy`]，与 epub 回写同形态）
//!
//! * `BilingualInline`：译文插在原块之后，一律 `<p class="docforge-tr">`
//!   （heading 后也是 p；`</li>` 后插入落在列表容器内，浏览器容忍）；
//!   表格格内在格内容终点插 `<br class="docforge-tr"/>` + 译文；
//! * `TranslationOnly`：块的首文本节点换译文、其余文本节点清空
//!   （Image/Code 结构块原样保留；Table 按格替换，见下）；
//! * `OriginalPlusAppendix`：正文不动，`</body>` 前追加 `<h2>译文</h2>` +
//!   译文段序列（无 body 的碎片件退化为文末追加；表格格译文不进附录，
//!   表格原样）。
//!
//! # 表格格级回写（wave2，与 epub 回写同构）
//!
//! 解析侧 `Collector` 对表格**无 colspan 展开**：`td`/`th` 按文档序各占一格
//! （列序 = 行内序），格文本 = 格内文本节点拼接后 trim（嵌套块级子元素只把
//! 文本并入格，不产块）。扫描侧镜像同一口径：[`HtmlCellSlot`] 按文档序收集
//! 非空格（trim 空格不占位），格序列与提取侧该表的格单元序列（`kind = Table`）
//! 同序同数——逐格顺序配对下一个格单元；格文本 ≠ 单元文本则跳过该格不写
//! （单元照常消费，宁漏勿错）。colspan 跨格文本因此落其文档序列位（如首列），
//! 不产生额外格位。

use std::path::Path;

use crate::emit::BiStrategy;
use crate::error::{DocError, Result};
use crate::model::BlockKind;
use crate::plugin::{FormatPlugin, ParseCapability};
use crate::plugins::html::{decode_entities, find_tag_end, heading_level, strip_comments, tag_name, SKIP_TAGS};
use crate::plugins::html::HtmlPlugin;
use crate::plugins::text::WholeTextParseCapability;

use super::{apply_edits, Edit, escape_xml_text, translation_map};

/// 译文样式类（BilingualInline 插入段与表格格内分隔符统一挂它，便于下游
/// 识别；epub 同值）。
pub(crate) const TR_CLASS: &str = "docforge-tr";

/// html 单文件双语回写：见模块级文档。`translated` 的 id 与
/// [`crate::emit::to_translation_units`] 同口径（`p0-b{块序}`）；
/// 未匹配 / 空白译文条目忽略；失败返回 `corrupt`。
pub fn writeback_html(
    in_path: &Path,
    translated: &[(String, String)],
    strategy: BiStrategy,
    out: &Path,
) -> Result<()> {
    let path_str = in_path
        .to_str()
        .ok_or_else(|| DocError::corrupt("html 路径不是 UTF-8"))?;

    // A. 权威解析（提取侧同管线、同默认选项）拿 id / 文本
    let raw = WholeTextParseCapability { id: "html.parse", fmt: "html" }.parse(path_str)?;
    let doc = HtmlPlugin.assemble(&raw);
    // 单元流全量（含表格格单元 kind = Table）：块级候选按块序消费、表格格
    // 在表格位逐格消费（见主循环的 Table 分支），两段消费拼满同一单元流。
    let units = crate::emit::to_translation_units(&doc);
    let map = translation_map(translated);

    // B. 读字节并解码：UTF-8（BOM 剥除后扫描、写回补回）；非 UTF-8 拒绝
    let bytes = std::fs::read(in_path)
        .map_err(|e| DocError::corrupt(format!("cannot read {}: {e}", in_path.display())))?;
    let (bom, text) = decode_utf8(&bytes)?;

    // C. 重块化扫描（parse_blocks 同口径，带 span）
    let (slots, body_close) = scan_html_blocks(&text);
    if doc.pages.len() != 1 {
        return Err(DocError::corrupt("html 对齐失败：Document 页数异常"));
    }
    if slots.len() != doc.pages[0].blocks.len() {
        return Err(DocError::corrupt("html 对齐失败：重扫描槽位数与解析块数不一致"));
    }

    // D. 块 × 单元按序配对（同一过滤口径）+ 一致性守卫，逐策略产编辑
    let page = &doc.pages[0];
    let mut edits: Vec<Edit> = Vec::new();
    let mut matched: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    for (b_idx, b) in page.blocks.iter().enumerate() {
        if b.kind == BlockKind::Table {
            // 表格格级回写（wave2）：扫描的非空格序列与提取侧该表的格单元
            // 序列同序同数（同一 Collector 口径），逐格顺序配对 + 文本守卫；
            // OriginalPlusAppendix 表格原样（格译文不进附录）。无 rows 结构
            // 的 Table 块防御性跳过（提取侧同样不发格单元）。
            if b.table.is_none() {
                continue;
            }
            for cell in &slots[b_idx].cells {
                let Some(unit) = units.get(cursor) else { break };
                cursor += 1;
                // 双保险：格文本 ≠ 单元文本 → 该格不写（宁漏勿错；单元照常消费）
                if unit.kind != BlockKind::Table || cell.text != unit.text {
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
                        // 格内容终点（闭标签前）插 <br class="docforge-tr"/> + 译文
                        edits.push(Edit::Insert(
                            cell.end,
                            format!("<br class=\"{TR_CLASS}\"/>{}", escape_xml_text(tr)),
                        ));
                    }
                    BiStrategy::TranslationOnly => {
                        // 格内首文本节点换译文、其余清空（与段落同款）
                        for (k, (s, e)) in cell.text_spans.iter().enumerate() {
                            if k == 0 {
                                edits.push(Edit::Replace(*s, *e, escape_xml_text(tr)));
                            } else {
                                edits.push(Edit::Replace(*s, *e, String::new()));
                            }
                        }
                    }
                    BiStrategy::OriginalPlusAppendix => {}
                }
            }
            continue;
        }
        let t = b.content.trim();
        if !crate::emit::is_translatable(b.kind) || t.is_empty() || t.chars().count() < 2 {
            continue;
        }
        let Some(unit) = units.get(cursor) else { break };
        cursor += 1;
        let slot = &slots[b_idx];
        // 双保险：重块化文本必须与权威单元文本一致
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
                // 插在块内容终点之后；无块回退的全文 span 钳进 </body> 前
                let mut pos = slot.end;
                if let Some(bc) = body_close {
                    pos = pos.min(bc);
                }
                edits.push(Edit::Insert(
                    pos,
                    format!("<p class=\"{TR_CLASS}\">{}</p>", escape_xml_text(tr)),
                ));
            }
            BiStrategy::TranslationOnly => {
                // 首文本节点换译文、其余清空；Image/Code 无文本 span，
                // 不进本分支（它们本就不是候选块；Table 走上面的格级分支）
                for (k, (s, e)) in slot.text_spans.iter().enumerate() {
                    if k == 0 {
                        edits.push(Edit::Replace(*s, *e, escape_xml_text(tr)));
                    } else {
                        edits.push(Edit::Replace(*s, *e, String::new()));
                    }
                }
            }
            BiStrategy::OriginalPlusAppendix => {}
        }
    }

    // Appendix：正文不动，</body> 前追加「译文」小节（碎片件退化到文末）
    if strategy == BiStrategy::OriginalPlusAppendix && !matched.is_empty() {
        let mut appendix = String::from("<h2>译文</h2>");
        for tr in &matched {
            appendix.push_str(&format!("<p>{}</p>", escape_xml_text(tr)));
        }
        let pos = body_close.unwrap_or(text.len());
        edits.push(Edit::Insert(pos, appendix));
    }

    // E. 改写并写回（无命中 = 原文原样；BOM 补回）
    let out_text = if edits.is_empty() {
        text
    } else {
        apply_edits(&text, edits)
    };
    let mut out_bytes = Vec::with_capacity(out_text.len() + 3);
    if bom {
        out_bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    out_bytes.extend_from_slice(out_text.as_bytes());
    std::fs::write(out, out_bytes)
        .map_err(|e| DocError::corrupt(format!("cannot create {}: {e}", out.display())))
}

/// UTF-8 严格解码：UTF-8 BOM 剥除（`(true, text)`），其余要求字节即 UTF-8；
/// 非 UTF-8 返回 `corrupt`（见模块级文档的编码取舍）。
fn decode_utf8(bytes: &[u8]) -> Result<(bool, String)> {
    let (bom, rest) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };
    match std::str::from_utf8(rest) {
        Ok(s) => Ok((bom, s.to_owned())),
        Err(_) => Err(DocError::corrupt(
            "html 非 UTF-8：回写仅支持 UTF-8 件（避免 charset 声明失配）",
        )),
    }
}

// ---------------------------------------------------------------------------
// XHTML 候选位扫描（parse_blocks 同口径，带字节 span；epub 回写共用）
// ---------------------------------------------------------------------------

/// 一个表格内非空格的定位（wave2 格级回写）：格文本 + 格内文本节点字节
/// span + 格内容终点。格文本与解析侧同口径（格内文本节点拼接后 trim，无
/// colspan 展开）；`end` = 闭标签 `<` 的偏移（BilingualInline 的
/// `<br class="docforge-tr"/>` + 译文插入点）。
#[derive(Debug, Clone)]
pub(crate) struct HtmlCellSlot {
    /// 格文本（= 解析侧 cell.trim()，提取侧格单元 text 同值）。
    pub(crate) text: String,
    /// 格内文本节点字节 span（TranslationOnly 首换译、其余清空）。
    pub(crate) text_spans: Vec<(usize, usize)>,
    /// 格内容终点（td/th 闭标签起点；BilingualInline 插位）。
    pub(crate) end: usize,
}

/// 一个 raw 块的定位信息：`parse_blocks` 会产出的块，逐一带字节 span。
#[derive(Debug, Clone)]
pub(crate) struct HtmlSlot {
    /// 块类型（与 parse_blocks 的 `Block.kind` 同值）。
    // epub 关闭时仅本文件读它（epub 回写的 TranslationOnly 过滤也读）——
    // 两个消费面不同 feature，任一单独编译都会有半数字段闲置。
    #[allow(dead_code)]
    pub(crate) kind: BlockKind,
    /// 块文本（= parse_blocks 的 `Block.content`，buf.trim()）。
    pub(crate) text: String,
    /// Heading 层级（切节判定用；非 Heading 为 None）。
    #[allow(dead_code)]
    pub(crate) level: Option<u8>,
    /// 块元素/内容的字节终点（BilingualInline 插位）。
    pub(crate) end: usize,
    /// 文本节点字节 span（TranslationOnly 用；Table/Image 为空）。
    pub(crate) text_spans: Vec<(usize, usize)>,
    /// 表格非空格序列（kind = Table 时）：文档序逐 tr → td/th，与提取侧
    /// 该表的格单元同序（见模块级「表格格级回写」）；其余块恒空。
    pub(crate) cells: Vec<HtmlCellSlot>,
}

/// 块扫描结果：候选位序列 + `</body>` 起始偏移（无则 None）。
pub(crate) fn scan_html_blocks(xhtml: &str) -> (Vec<HtmlSlot>, Option<usize>) {
    scan_impl(xhtml)
}

/// 与 [`parse_blocks`] 完全同款的块触发顺序 / 空白 / 实体口径，附带 span 与
/// 文本节点位置。块元素 span = 开标签起（无内容时）到闭标签止。
fn scan_impl(xhtml: &str) -> (Vec<HtmlSlot>, Option<usize>) {
    // Collector 状态（对齐 plugins/html.rs）
    let mut buf = String::new();
    let mut cell = String::new();
    let mut row: Option<Vec<String>> = None;
    let mut table: Option<Vec<Vec<String>>> = None;
    let mut heading: Option<u8> = None;
    let mut skip_depth = 0usize;
    let mut list_stack: Vec<bool> = Vec::new();

    // 格级回写状态（wave2）：td/th 开..闭窗口内的文本节点 span 进当前格；
    // 非空格按文档序进 cells（与提取侧格单元同序，见模块级「表格格级回写」）
    let mut cell_spans: Vec<(usize, usize)> = Vec::new();
    let mut in_cell = false;
    let mut cells: Vec<HtmlCellSlot> = Vec::new();

    // span 状态
    let mut slots: Vec<HtmlSlot> = Vec::new();
    let mut body_close: Option<usize> = None;
    let mut content_start: Option<usize> = None;
    let mut content_end = 0usize;
    let mut cur_spans: Vec<(usize, usize)> = Vec::new();

    macro_rules! flush {
        ($kind:expr, $level:expr, $trig:expr) => {
            flush_block(
                &mut slots,
                &mut buf,
                &mut cur_spans,
                &mut content_start,
                &mut content_end,
                $kind,
                $level,
                $trig,
            )
        };
    }

    let bytes = xhtml.as_bytes();
    let mut i = 0usize;
    let mut text_start = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if text_start < i {
            // 文本节点（与 Collector.on_text 同口径：解实体、表格内进单元格）
            let data = &xhtml[text_start..i];
            let decoded = decode_entities(data);
            if skip_depth == 0 && !decoded.is_empty() {
                if content_start.is_none() {
                    content_start = Some(text_start);
                }
                content_end = i;
                // 格内 span 归当前格（格级回写），其余归挂起块
                if in_cell {
                    cell_spans.push((text_start, i));
                } else {
                    cur_spans.push((text_start, i));
                }
                if row.is_some() {
                    cell.push_str(&decoded);
                } else {
                    buf.push_str(&decoded);
                }
            }
        }
        if xhtml[i..].starts_with("<!--") {
            i = match xhtml[i + 4..].find("-->") {
                Some(rel) => i + 4 + rel + 3,
                None => bytes.len(),
            };
            text_start = i;
            continue;
        }
        let tag_start = i;
        match find_tag_end(&xhtml[i..]) {
            Some(rel) => {
                let raw_tag = &xhtml[i + 1..i + rel];
                let tag_end = i + rel + 1;
                let trimmed = raw_tag.trim();
                if let Some(close) = trimmed.strip_prefix('/') {
                    let name = tag_name(close);
                    if SKIP_TAGS.contains(&name.as_str()) {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if skip_depth == 0 {
                        // 闭标签推进内容终点（元素级 span 的收口）
                        content_end = tag_end;
                        if heading_level(&name).is_some() {
                            let lvl = heading;
                            flush!(BlockKind::Heading, lvl, (tag_start, tag_end));
                            heading = None;
                        } else {
                            match name.as_str() {
                                "ol" | "ul" => {
                                    list_stack.pop();
                                }
                                "pre" => flush!(BlockKind::Code, None, (tag_start, tag_end)),
                                "li" => flush!(BlockKind::List, None, (tag_start, tag_end)),
                                "td" | "th" => {
                                    // 格文本与解析侧同口径（cell.trim()）；非空格
                                    // 占一个格位（wave2），空格连 span 一并丢弃
                                    let text = cell.trim().to_string();
                                    if let Some(r) = row.as_mut() {
                                        r.push(text.clone());
                                    }
                                    if text.is_empty() {
                                        cell_spans.clear();
                                    } else {
                                        cells.push(HtmlCellSlot {
                                            text,
                                            text_spans: std::mem::take(&mut cell_spans),
                                            end: tag_start,
                                        });
                                    }
                                    in_cell = false;
                                    cell.clear();
                                }
                                "tr" => {
                                    if let (Some(t), Some(r)) = (table.as_mut(), row.take()) {
                                        t.push(r);
                                    }
                                }
                                "table" => {
                                    // cells 整表收割（rows 为空不成块也照收，避免
                                    // 泄漏进下一个表）；杂散格窗口复位
                                    let table_cells = std::mem::take(&mut cells);
                                    in_cell = false;
                                    cell_spans.clear();
                                    if let Some(rows) = table.take() {
                                        if !rows.is_empty() {
                                            slots.push(HtmlSlot {
                                                kind: BlockKind::Table,
                                                text: rows[0].join(" | "),
                                                level: None,
                                                end: tag_end,
                                                text_spans: Vec::new(),
                                                cells: table_cells,
                                            });
                                        }
                                    }
                                }
                                "p" | "div" => {
                                    flush!(BlockKind::Paragraph, None, (tag_start, tag_end))
                                }
                                "blockquote" => {
                                    flush!(BlockKind::Quote, None, (tag_start, tag_end))
                                }
                                "body" => body_close = Some(tag_start),
                                _ => {}
                            }
                        }
                    }
                } else if !trimmed.is_empty()
                    && !trimmed.starts_with('!')
                    && !trimmed.starts_with('?')
                {
                    let name = tag_name(trimmed);
                    if name == "img" {
                        // void 元素（修补②同款）：先封口挂起文本，再产 Image 块
                        flush!(BlockKind::Paragraph, None, (tag_start, tag_start));
                        slots.push(HtmlSlot {
                            kind: BlockKind::Image,
                            text: String::new(),
                            level: None,
                            end: tag_end,
                            text_spans: Vec::new(),
                            cells: Vec::new(),
                        });
                    } else if SKIP_TAGS.contains(&name.as_str()) {
                        skip_depth += 1;
                    } else if skip_depth == 0 {
                        if let Some(level) = heading_level(&name) {
                            flush!(BlockKind::Paragraph, None, (tag_start, tag_start));
                            heading = Some(level);
                        } else {
                            match name.as_str() {
                                "ol" => list_stack.push(true),
                                "ul" => list_stack.push(false),
                                "p" | "div" | "br" | "li" | "blockquote" => {
                                    flush!(BlockKind::Paragraph, None, (tag_start, tag_start))
                                }
                                "pre" => {
                                    flush!(BlockKind::Paragraph, None, (tag_start, tag_start))
                                }
                                "table" => {
                                    flush!(BlockKind::Paragraph, None, (tag_start, tag_start));
                                    table = Some(Vec::new());
                                    // 新表开新格序列（嵌套/杂散表的格不串表）
                                    cells.clear();
                                    in_cell = false;
                                    cell_spans.clear();
                                }
                                "tr" => {
                                    if table.is_some() {
                                        row = Some(Vec::new());
                                    }
                                }
                                "td" | "th" => {
                                    cell.clear();
                                    // 格窗口开启（仅表内——表外杂散 td 的文本
                                    // 走 buf，span 留在挂起块，与解析侧一致）
                                    if row.is_some() {
                                        in_cell = true;
                                        cell_spans.clear();
                                    }
                                }
                                _ => {}
                            }
                        }
                        // 未封口块的内容起点：元素开标签也算贡献起点
                        if content_start.is_none() {
                            content_start = Some(tag_start);
                        }
                    }
                }
                i = tag_end;
            }
            None => i = bytes.len(),
        }
        text_start = i;
    }
    if text_start < bytes.len() {
        let data = &xhtml[text_start..];
        let decoded = decode_entities(data);
        if skip_depth == 0 && !decoded.is_empty() {
            if content_start.is_none() {
                content_start = Some(text_start);
            }
            content_end = bytes.len();
            if in_cell {
                cell_spans.push((text_start, bytes.len()));
            } else {
                cur_spans.push((text_start, bytes.len()));
            }
            if row.is_some() {
                cell.push_str(&decoded);
            } else {
                buf.push_str(&decoded);
            }
        }
    }
    // finish()
    flush!(BlockKind::Paragraph, None, (bytes.len(), bytes.len()));

    // parse_blocks 的无块回退：整篇归一化纯文本成一段（span = 全文，
    // 插位由调用方钳进 </body> 前）
    if slots.is_empty() {
        let plain = strip_comments(xhtml)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !plain.is_empty() {
            slots.push(HtmlSlot {
                kind: BlockKind::Paragraph,
                text: plain,
                level: None,
                end: xhtml.len(),
                text_spans: vec![(0, xhtml.len())],
                cells: Vec::new(),
            });
        }
    }
    (slots, body_close)
}

/// 封口挂起块（与 `Collector.flush` 同口径：空白不产出；块文本 = buf.trim()）。
/// span：元素块 =（内容起点, 闭标签止）；文本/触发标签块 =（内容起点, 内容终点）。
#[allow(clippy::too_many_arguments)]
fn flush_block(
    slots: &mut Vec<HtmlSlot>,
    buf: &mut String,
    cur_spans: &mut Vec<(usize, usize)>,
    content_start: &mut Option<usize>,
    content_end: &mut usize,
    kind: BlockKind,
    level: Option<u8>,
    trigger: (usize, usize),
) {
    let text = buf.trim().to_string();
    buf.clear();
    let spans = std::mem::take(cur_spans);
    let end = (*content_end).max(trigger.1);
    *content_start = None;
    *content_end = 0;
    if text.is_empty() {
        return; // 与 Collector.flush 一致：空白块不产出
    }
    slots.push(HtmlSlot {
        kind,
        text,
        level,
        end,
        text_spans: spans,
        cells: Vec::new(),
    });
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::emit::to_translation_units;
    use crate::model::BlockKind::{Heading, List, Paragraph, Quote, Table};
    use crate::plugins::html::parse_blocks;

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_html_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn parse_doc(path: &Path) -> crate::model::Document {
        let raw = WholeTextParseCapability { id: "html.parse", fmt: "html" }
            .parse(path.to_str().unwrap())
            .unwrap();
        HtmlPlugin.assemble(&raw)
    }

    /// 回写消费的单元 id（块级候选 + 表格格单元，wave2 起与提取侧全量
    /// 单元流一致）。
    fn unit_ids(path: &Path) -> Vec<String> {
        to_translation_units(&parse_doc(path))
            .into_iter()
            .map(|u| u.id)
            .collect()
    }

    fn write(in_path: &Path, out_path: &Path, tr: &[(&str, &str)], strategy: BiStrategy) {
        let translated: Vec<(String, String)> =
            tr.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        writeback_html(in_path, &translated, strategy, out_path).unwrap();
    }

    fn out_str(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// 金测（核心验收）：BilingualInline 往返 —— 块序原/译交错、实体转义
    /// 往返、表格与 1 字符短段不受扰、未匹配 id 忽略。
    #[test]
    fn roundtrip_bilingual_inline() {
        let dir = case_dir("inline");
        let in_path = dir.join("in.html");
        std::fs::write(
            &in_path,
            "<html><head><title>t</title></head><body>\
             <h1>第一章</h1><p>正文一句。</p><p>乙</p>\
             <ul><li>要点一。</li><li>要点二。</li></ul>\
             <blockquote>引文一句。</blockquote>\
             <table><tr><td>格甲</td><td>格乙</td></tr></table>\
             </body></html>",
        )
        .unwrap();

        // 回写消费 id 口径（短段乙占 b2 不进流；表格占 b6：格单元 wave2 起
        // 在表格位消费，本测未给格译文 → 表格原样）
        assert_eq!(
            unit_ids(&in_path),
            [
                "p0-b0",
                "p0-b1",
                "p0-b3",
                "p0-b4",
                "p0-b5",
                "p0-b6-r0c0",
                "p0-b6-r0c1"
            ]
        );

        let out_path = dir.join("out.html");
        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "Chapter 1"),
                ("p0-b1", "First sentence."),
                ("p0-b3", "Point one."),
                ("p0-b4", "Point two."),
                ("p0-b5", "A quote."),
                ("p0-b99", "bogus"), // 未匹配 id：忽略
            ],
            BiStrategy::BilingualInline,
        );

        let out = out_str(&out_path);
        assert!(out.contains("<p class=\"docforge-tr\">Chapter 1</p>"));
        assert!(out.contains("格甲"), "表格不受扰");
        assert!(!out.contains("bogus"), "未匹配 id 不落盘");

        // 重解析：原/译交错，块序不变。译文一律插 `<p class="docforge-tr">`，
        // heading 后也是 p（与 epub 回写同款简化，译文不借用标题层级）
        let blocks = parse_blocks(&out);
        let kinds: Vec<BlockKind> = blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                Heading, Paragraph, // h1 + 译段（p）
                Paragraph, Paragraph, // 原段 + 译段
                Paragraph, // 短段乙原样
                List, Paragraph, List, Paragraph, // li 原/译交错
                Quote, Paragraph, // 引文原 + 译
                Table, // 表格原样
            ]
        );
        let contents: Vec<&str> = blocks.iter().map(|b| b.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "第一章", "Chapter 1",
                "正文一句。", "First sentence.",
                "乙",
                "要点一。", "Point one.", "要点二。", "Point two.",
                "引文一句。", "A quote.",
                "格甲 | 格乙",
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TranslationOnly：首文本节点换译文、其余文本节点清空；未翻译块原样。
    #[test]
    fn translation_only_replaces_text() {
        let dir = case_dir("tonly");
        // 实体拆 span 考点：&amp; 把段落文本拆成 3 个文本节点
        let in_path = dir.join("in.html");
        std::fs::write(
            &in_path,
            "<html><body><h1>第一章</h1>\
             <p>含 &amp; 实体的一段。</p><p>保留原文。</p></body></html>",
        )
        .unwrap();
        let out_path = dir.join("out.html");
        write(
            &in_path,
            &out_path,
            &[("p0-b0", "Chapter 1"), ("p0-b1", "Entity & kept.")],
            BiStrategy::TranslationOnly,
        );

        let blocks = parse_blocks(&out_str(&out_path));
        assert_eq!(blocks.len(), 3, "无插入：块数与原文一致");
        assert_eq!(blocks[0].kind, Heading);
        assert_eq!(blocks[0].content, "Chapter 1");
        assert_eq!(blocks[1].content, "Entity & kept.", "多 span：首换译、其余清空");
        assert_eq!(blocks[2].content, "保留原文。", "未翻译单元原样");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：正文不动，</body> 前追加 h2「译文」+ 译文段；
    /// 全部未匹配时输出与原文逐块一致（透传）。
    #[test]
    fn appendix_appends_before_body_close() {
        let dir = case_dir("appendix");
        let in_path = dir.join("in.html");
        std::fs::write(
            &in_path,
            "<html><body><h1>第一章</h1><p>正文一句。</p></body></html>",
        )
        .unwrap();
        let out_path = dir.join("out.html");
        write(
            &in_path,
            &out_path,
            &[("p0-b0", "Chapter 1"), ("p0-b1", "First sentence.")],
            BiStrategy::OriginalPlusAppendix,
        );
        let out = out_str(&out_path);
        let body_end = out.rfind("</body>").unwrap();
        let h2 = out.find("<h2>译文</h2>").unwrap();
        assert!(h2 < body_end, "附录在 </body> 之前");
        assert!(out.contains("<p>Chapter 1</p>"));
        assert!(out.contains("<p>正文一句。</p>"), "正文不动");
        let doc = parse_doc(&out_path);
        let kinds: Vec<BlockKind> = doc.pages[0].blocks.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, vec![Heading, Paragraph, Heading, Paragraph, Paragraph]);
        assert_eq!(doc.pages[0].blocks[2].content, "译文");
        assert_eq!(doc.pages[0].blocks[3].content, "Chapter 1");

        // 全部未匹配：透传
        let out2 = dir.join("out2.html");
        write(&in_path, &out2, &[("p0-b42", "nope")], BiStrategy::BilingualInline);
        assert_eq!(parse_doc(&out2), parse_doc(&in_path));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非 UTF-8 件拒绝（避免 charset 声明失配）；UTF-8 BOM 剥除后扫描、写回补回。
    #[test]
    fn utf8_only_with_bom_preserved() {
        let dir = case_dir("enc");
        // 非 UTF-8：拒绝
        let bad = dir.join("bad.html");
        std::fs::write(&bad, [0xFF, 0xFE, b'a', b'b']).unwrap();
        let err = writeback_html(
            &bad,
            &[("p0-b0".to_string(), "x".to_string())],
            BiStrategy::BilingualInline,
            &dir.join("bad-out.html"),
        )
        .unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");

        // UTF-8 BOM：BOM 不参与扫描（与解析侧 decode 剥 BOM 同口径），写回补回
        let good = dir.join("bom.html");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("<html><body><p>正文。</p></body></html>".as_bytes());
        std::fs::write(&good, &bytes).unwrap();
        assert_eq!(unit_ids(&good), ["p0-b0"]);
        let out_path = dir.join("bom-out.html");
        write(&good, &out_path, &[("p0-b0", "Body.")], BiStrategy::BilingualInline);
        let raw = std::fs::read(&out_path).unwrap();
        assert!(raw.starts_with(&[0xEF, 0xBB, 0xBF]), "BOM 补回");
        assert!(
            String::from_utf8_lossy(&raw)
                .contains(r#"正文。</p><p class="docforge-tr">Body.</p>"#),
            "译文紧贴原段"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 表格格级回写（wave2 金测）：TranslationOnly 全格替换——格单元全译后
    /// 重解析，格文本 == 译文；空格不发单元（重解析仍为空）；表头 th 同格
    /// 口径；单字符格不受 min_chars 洗刷；实体拆多 span 的格首换译、其余清空；
    /// 段落/标题不受表格波及。
    #[test]
    fn table_cells_translation_only_roundtrip() {
        let dir = case_dir("cell-tonly");
        let in_path = dir.join("in.html");
        std::fs::write(
            &in_path,
            "<html><body><h1>参数表</h1>\
             <table><tr><th>名称</th><th>单位</th></tr>\
             <tr><td>电压</td><td></td></tr>\
             <tr><td>电流 &amp; 漏电</td><td>安</td></tr></table>\
             <p>表格之后一段。</p></body></html>",
        )
        .unwrap();

        // 提取侧：表格按格展开，空格 r1c1 不发单元，格阈值恒 1（“安”进流）
        assert_eq!(
            unit_ids(&in_path),
            [
                "p0-b0",
                "p0-b1-r0c0",
                "p0-b1-r0c1",
                "p0-b1-r1c0",
                "p0-b1-r2c0",
                "p0-b1-r2c1",
                "p0-b2",
            ]
        );

        let out_path = dir.join("out.html");
        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "Spec table"),
                ("p0-b1-r0c0", "Name"),
                ("p0-b1-r0c1", "Unit"),
                ("p0-b1-r1c0", "Voltage"),
                ("p0-b1-r2c0", "Current & leakage"),
                ("p0-b1-r2c1", "A"),
                ("p0-b2", "After the table."),
            ],
            BiStrategy::TranslationOnly,
        );

        let blocks = parse_blocks(&out_str(&out_path));
        assert_eq!(blocks.len(), 3, "无插入：块数不变");
        assert_eq!(blocks[0].content, "Spec table");
        let rows = &blocks[1].table.as_ref().unwrap().rows;
        assert_eq!(
            rows,
            &vec![
                vec!["Name".to_string(), "Unit".to_string()],
                vec!["Voltage".to_string(), String::new()],
                vec!["Current & leakage".to_string(), "A".to_string()],
            ]
        );
        assert_eq!(blocks[2].content, "After the table.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// colspan 对齐（wave2）：解析侧无 colspan 展开——跨列文本落文档序列位
    /// (r,0)、续列占 (r,1)、无额外格位；只给跨列格译文时续列原样（部分命中），
    /// 给续列译文时同样正确落格（顺序配对 + 文本守卫对 colspan 透明）。
    #[test]
    fn table_cells_colspan_alignment() {
        let dir = case_dir("cell-colspan");
        let in_path = dir.join("in.html");
        std::fs::write(
            &in_path,
            "<html><body><table>\
             <tr><td colspan=\"2\">跨列文本</td><td>续列</td></tr>\
             <tr><td>甲格</td><td>乙格</td></tr></table></body></html>",
        )
        .unwrap();

        // 提取侧网格按文档序：跨列文本占 (r,0)、续列占 (r,1)——无展开
        assert_eq!(
            unit_ids(&in_path),
            ["p0-b0-r0c0", "p0-b0-r0c1", "p0-b0-r1c0", "p0-b0-r1c1"]
        );

        // 部分命中：只给跨列格译文 → 跨列格替换、续列原样（续列无译文位）
        let out_path = dir.join("out.html");
        write(
            &in_path,
            &out_path,
            &[("p0-b0-r0c0", "Spanning")],
            BiStrategy::TranslationOnly,
        );
        let blocks = parse_blocks(&out_str(&out_path));
        let rows = &blocks[0].table.as_ref().unwrap().rows;
        assert_eq!(
            rows,
            &vec![
                vec!["Spanning".to_string(), "续列".to_string()],
                vec!["甲格".to_string(), "乙格".to_string()],
            ]
        );

        // 全命中：续列译文同样正确落格
        let out2 = dir.join("out2.html");
        write(
            &in_path,
            &out2,
            &[
                ("p0-b0-r0c0", "Spanning"),
                ("p0-b0-r0c1", "Follow-up"),
                ("p0-b0-r1c0", "One"),
                ("p0-b0-r1c1", "Two"),
            ],
            BiStrategy::TranslationOnly,
        );
        let blocks2 = parse_blocks(&out_str(&out2));
        let rows2 = &blocks2[0].table.as_ref().unwrap().rows;
        assert_eq!(
            rows2,
            &vec![
                vec!["Spanning".to_string(), "Follow-up".to_string()],
                vec!["One".to_string(), "Two".to_string()],
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BilingualInline 格内对照：`<br class="docforge-tr"/>` + 译文插在格内容
    /// 终点（不拆元素）；重解析格文本为原/译直连拼接（解析侧 `<br>` 只封口
    /// 段落缓冲、不拆格文本——断言按实测口径）；未命中格原样。
    #[test]
    fn table_cells_bilingual_inline_br() {
        let dir = case_dir("cell-inline");
        let in_path = dir.join("in.html");
        std::fs::write(
            &in_path,
            "<html><body><table><tr><td>格甲</td><td>格乙</td></tr></table></body></html>",
        )
        .unwrap();
        let out_path = dir.join("out.html");
        write(
            &in_path,
            &out_path,
            &[("p0-b0-r0c0", "Cell A")],
            BiStrategy::BilingualInline,
        );

        let out = out_str(&out_path);
        assert!(
            out.contains("<td>格甲<br class=\"docforge-tr\"/>Cell A</td>"),
            "格内 br + 译文插在格内容终点：{out}"
        );
        let blocks = parse_blocks(&out);
        let rows = &blocks[0].table.as_ref().unwrap().rows;
        assert_eq!(
            rows,
            &vec![vec!["格甲Cell A".to_string(), "格乙".to_string()]],
            "重解析：格文本原/译直连（br 不拆格），未命中格原样"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：表格格译文不进附录、表格原样；段落译文照常进附录。
    #[test]
    fn table_cells_appendix_excluded() {
        let dir = case_dir("cell-appendix");
        let in_path = dir.join("in.html");
        std::fs::write(
            &in_path,
            "<html><body><p>正文一段。</p>\
             <table><tr><td>格甲</td><td>格乙</td></tr></table></body></html>",
        )
        .unwrap();
        let out_path = dir.join("out.html");
        write(
            &in_path,
            &out_path,
            &[("p0-b0", "Body paragraph."), ("p0-b1-r0c0", "Cell A")],
            BiStrategy::OriginalPlusAppendix,
        );

        let out = out_str(&out_path);
        assert!(out.contains("<p>Body paragraph.</p>"), "段落译文进附录");
        assert!(out.contains("格甲"), "表格原样");
        assert!(!out.contains("Cell A"), "格译文不进附录：{out}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
