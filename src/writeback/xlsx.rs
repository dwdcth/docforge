// docforge 新增（翻译闭环③写侧）：xlsx 双语回写（补齐表格翻译对 .xlsx 的容器出口）。
//! XLSX 双语回写：解包原 ZIP → 重解析建立权威格单元流 → 逐 sheet 扫描 `<c>` 格位
//! 按文档序配对 → 命中格改写为 inlineStr → 其余部件字节透传，重打包到 `out`。
//!
//! # 对齐口径（与 `backends/office/xlsx.rs` 解析侧逐条镜像）
//!
//! * 单元流：xlsx 的一 sheet = 一页一表（Table 块恒在块序 0），格单元锚点
//!   `p{sheet 名}-b0-r{行}c{列}`（[`crate::emit::to_translation_units`]，格阈值
//!   恒 1 = trim 非空即收；include_regions 默认关 → 页面只有格单元）。回写侧
//!   重解析用同一后端同一默认选项（mark 合并策略 / 不截断 / 不读批注）。
//! * 行列映射（关键镜像点）：解析侧默认 `merge_cell_policy = mark` 不展开合并
//!   区，`TableData.rows` 的行 = sheet XML 里**非全空 `<row>` 的文档序**（行号
//!   取 `r` 属性、缺省按序补位——空行不占行位但序号继续走），列 = 行内 `<c>`
//!   按 `r` 引用列号**升序排稳后的下标**（`<c>` 缺 `r` 引用时列 = 上一格 +1；
//!   自闭合 `<c r="A2"/>` 无内容，解析侧不产格、此处也不占列）。格位扫描按同
//!   一套规则给每个 `<c>` 配 (行, 列) 紧凑下标，即与单元流一一对位——不需要
//!   A1 坐标反解，格元素自带的 `r` 引用就是写回落点。merges 展开格不存在：
//!   mark 策略下合并区只是 `meta["merges"]` 标注，左上角之外的格没有文本就没有
//!   单元没有格位，自然不会误写。
//! * 文本口径（守卫基准）：字符格与解析侧逐字节同口径——t="s"（及 t 缺省，
//!   解析侧默认按共享串解）按 `xl/sharedStrings.xml` 下标解引用（`<si>` 文本
//!   trim，实体 / CDATA 同解），t="inlineStr"/"str" 取 `<is>`/`<v>` 文本 trim。
//!   数值（t="n"）/布尔/错误格此处只留 **XML 原文形态**（如 `<v>45292</v>` 的
//!   `"45292"`）作占位——与解析侧的格式化串（日期 `"01-01-24"`、数字规整）天然
//!   不等，文本守卫自动跳过；且这类格**结构性不可写**（见下），双重防线。
//! * 可写判据：只有字符串格（共享串 / 内联串）写译文——数值/布尔/错误格改写
//!   成文本会把数值格变文本格（对齐、公式引用、SUM 语义全变），宁可漏写也不
//!   错写。文本守卫拦口径漂移，可写判据拦类型误写；格位照常占位保流对齐。
//! * 双保险：每个格位与重解析的对应单元做文本一致性比对，不一致即跳过该格
//!   （单元照常消费保流对齐）——宁漏勿错；`translated` 里对不上任何单元 id 的
//!   条目一律忽略（不报错），空白译文视同未提供。
//!
//! # 为什么写 inlineStr（而不是改 sharedStrings.xml）
//!
//! 同一共享串常被多格引用（表头跨 sheet、重复值列），改 `<si>` 是全表连锁
//! 改写，译文会串进所有引用格。改为把目标格整体替换成
//! `<c r="…" s="…" t="inlineStr"><is><t>译文</t></is></c>`：影响局部于该
//! `<c>` 元素，同串其他引用格不受扰，`sharedStrings.xml` 字节不动。`r` 引用
//! 与 `s`（样式：字体/边框/自动换行）保留（缺 `r` 的格按行列换算补名），原
//! `t` 与 `<v>`/`<is>`/
//! `<f>` 内容被译文替换——字符串型公式格命中时公式随格消失（译文已固化，
//! 公式重算反而会盖掉译文）。
//!
//! # 三种策略（[`crate::emit::BiStrategy`]）
//!
//! * `BilingualInline`（默认）：格内 `原文\n译文`（同 inlineStr 写回；Excel 需
//!   开启「自动换行」才显示两行，是否补 wrap 样式不强改——改样式超出「定位
//!   改写」的边界）；
//! * `TranslationOnly`：格文本替换为译文；
//! * `OriginalPlusAppendix`：正文格一律不动，追加「译文」sheet（表头行
//!   `单元格 / 原文 / 译文`，数据行 `『sheet 名!A1』 / 原文 / 译文`；sheet 名
//!   按引用规则需要引号时加单引号）。落地 = 新建 worksheet 部件 +
//!   workbook.xml 追 `<sheet>` 声明 + workbook rels 追关系 +
//!   `[Content_Types].xml` 追 override；sheet 名与既有名冲突时依次退让
//!   （`译文`、`译文2`、`译文3`…）；无任何命中译文时整个追加不发生（输出 =
//!   原包透传）。
//!
//! # 覆盖与门控
//!
//! 随 feature `xlsx`（复用解析侧管线，无新依赖）；xlsx 的 .md 出口走
//! [`crate::emit::to_bilingual_markdown`] 纯文本重组，本模块补齐 .xlsx 容器出口。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::backends::office::xlsx::XlsxRawBackend;
use crate::backends::office::{open_zip, rel_entries, resolve_part, tags, zip_read_str, OfficePlugin};
use crate::emit::BiStrategy;
use crate::error::{DocError, Result};
use crate::plugin::{FormatPlugin as _, ParseCapability as _};

use super::{apply_edits, escape_xml_text, translation_map, Edit};

/// xlsx 双语回写：见模块级文档。`translated` 的 id 与
/// [`crate::emit::to_translation_units`] 同口径（格 `p{sheet}-b{块序}-r{行}c{列}`）；
/// 未匹配 / 空白译文条目忽略；失败返回 `corrupt`。
pub fn writeback_xlsx(
    in_path: impl AsRef<Path>,
    translated: &[(String, String)],
    strategy: BiStrategy,
    out: impl AsRef<Path>,
) -> Result<()> {
    let in_path = in_path.as_ref();
    let out = out.as_ref();

    // 第一遍：docforge 重解析（默认装配：mark 合并策略 / 不截断 / 不读批注）
    // 建立权威格单元流（xlsx 一 sheet = 一页一表，页面只有格单元）。
    let raw = XlsxRawBackend::default().parse(in_path)?;
    let doc = OfficePlugin::default().assemble(&raw);
    let units = crate::emit::to_translation_units(&doc);
    let map = translation_map(translated);

    // 第二遍：ZIP 内逐 sheet 扫描格位，与单元流按文档序配对。
    let mut zip = open_zip(in_path)?;
    let entry_names: Vec<String> = (0..zip.len())
        .filter_map(|i| {
            zip.by_index(i)
                .ok()
                .and_then(|f| f.name().ok().map(|n| n.to_string()))
        })
        .collect();
    let wb_xml = zip_read_str(&mut zip, "xl/workbook.xml")?
        .ok_or_else(|| DocError::corrupt("missing xl/workbook.xml"))?;
    let sheets = sheet_entries(&wb_xml)?;
    // sheet 部件名走 workbook rels（r:id → Target），不靠 `sheetN.xml` 序号猜
    // （解析侧同口径；rel 缺失回退序号命名）
    let rels_xml = zip_read_str(&mut zip, "xl/_rels/workbook.xml.rels")?;
    let rels = rels_xml.as_deref().map(rel_entries).unwrap_or_default();
    let parts: Vec<String> = sheets
        .iter()
        .enumerate()
        .map(|(idx, (rid, _))| {
            rels.iter()
                .find(|(rel_id, rtype, _)| rel_id == rid && rtype.ends_with("/worksheet"))
                .map(|(_, _, target)| resolve_part("xl", target))
                .unwrap_or_else(|| format!("xl/worksheets/sheet{}.xml", idx + 1))
        })
        .collect();

    let sst = SharedStrings::load(&mut zip)?;
    let ct_xml = zip_read_str(&mut zip, "[Content_Types].xml")?;
    let mut sheet_xmls: Vec<String> = Vec::with_capacity(parts.len());
    let mut slots: Vec<CellSlot> = Vec::new();
    for (idx, part) in parts.iter().enumerate() {
        let xml = zip_read_str(&mut zip, part)?.unwrap_or_default();
        slots.extend(scan_sheet_slots(&xml, idx, &sst)?);
        sheet_xmls.push(xml);
    }
    drop(zip);

    let sheet_names: Vec<String> = sheets.iter().map(|(_, name)| name.clone()).collect();
    let matched = matched_translations(&slots, &units, &map);

    match strategy {
        BiStrategy::BilingualInline | BiStrategy::TranslationOnly => {
            // BilingualInline = 格内 `原文\n译文`；TranslationOnly = 纯译文。
            // 两者都是整格替换为 inlineStr，只是 `<t>` 内容不同。
            let inline = strategy == BiStrategy::BilingualInline;
            let mut by_sheet: BTreeMap<usize, Vec<Edit>> = BTreeMap::new();
            for (k, tr) in &matched {
                let slot = &slots[*k];
                if !slot.writable {
                    continue; // 数值/日期/布尔/错误格：结构性不可写（见模块头）
                }
                let text = if inline {
                    format!("{}\n{}", units[*k].text.trim(), tr)
                } else {
                    tr.clone()
                };
                by_sheet
                    .entry(slot.sheet)
                    .or_default()
                    .push(Edit::Replace(slot.span.0, slot.span.1, inline_str_cell(slot, &text)));
            }
            let replaced: Vec<(String, Vec<u8>)> = by_sheet
                .into_iter()
                .map(|(idx, edits)| {
                    (parts[idx].clone(), apply_edits(&sheet_xmls[idx], edits).into_bytes())
                })
                .collect();
            rewrite_zip_with_added(in_path, out, &replaced, &[])
        }
        BiStrategy::OriginalPlusAppendix => {
            // 附录只收真正写得出译文的格（可写字符串格且文本守卫命中）；
            // 正文格一律原样。
            let rows: Vec<(String, String, String)> = matched
                .iter()
                .filter(|(k, _)| slots[*k].writable)
                .map(|(k, tr)| {
                    let slot = &slots[*k];
                    let name = sheet_names.get(slot.sheet).map_or("", |s| s.as_str());
                    (
                        sheet_cell_ref(name, &slot.a1),
                        units[*k].text.trim().to_string(),
                        tr.clone(),
                    )
                })
                .collect();
            if rows.is_empty() {
                // 无命中译文：输出 = 原包透传（对齐 docx 附录空命中不追加）
                return rewrite_zip_with_added(in_path, out, &[], &[]);
            }
            let (replaced, added) =
                build_appendix(&entry_names, &wb_xml, rels_xml.as_deref(), ct_xml.as_deref(), &sheet_names, &rows)?;
            rewrite_zip_with_added(in_path, out, &replaced, &added)
        }
    }
}

// ---------------------------------------------------------------------------
// 格位扫描（quick-xml 流式，带字节 span；与解析侧 parse_sheet_rows 同序同口径）
// ---------------------------------------------------------------------------

/// 一个候选格位：与格单元流按文档序一一对位。
#[derive(Debug, Clone)]
struct CellSlot {
    /// sheet 序号（workbook 顺序；编辑分发到对应 sheet XML 用）。
    sheet: usize,
    /// `<c …>…</c>` 的字节区间（整个元素；改写即整格替换为 inlineStr）。
    span: (usize, usize),
    /// 原 `s`（样式）属性值——改写保留（字体/边框/自动换行不丢）。
    s_attr: Option<String>,
    /// 与解析侧同口径的格文本（字符格 = SST / 内联串；数值/布尔/错误格 = XML
    /// 原文形态占位——只用于守卫与空白判定，绝不写回）。
    text: String,
    /// 字符串格（t 缺省 / s / inlineStr / str）才可写；数值/布尔/错误格写入会
    /// 把数值格变文本格，宁可跳过（见模块头「可写判据」）。
    writable: bool,
    /// A1 引用（原 `r` 属性或缺 `r` 格按行列换算；附录 sheet 的「单元格」列用）。
    a1: String,
}

/// 扫描中的格累积器（Start `<c>` 到 End `</c>` 之间的属性与文本值）。
struct CellAcc {
    start: usize,
    r_attr: Option<String>,
    s_attr: Option<String>,
    /// 解析侧同款单字母类型：s=共享串（t 缺省同此）、i=内联串、n=数字、
    /// b=布尔、e=错误。
    ty: char,
    value: String,
}

/// 行内已收格（文档序；行收尾时按列号升序排稳）。
struct RawCell {
    col: usize,
    text: String,
    span: (usize, usize),
    r_attr: Option<String>,
    s_attr: Option<String>,
    writable: bool,
}

/// 逐 sheet 扫描候选格位。事件结构与文本口径同解析侧 `parse_sheet_rows`
/// 逐条镜像（含 t 缺省按共享串解、`<c>` 缺 `r` 引用列 = 上一格 +1、自闭合
/// `<c/>` 不产格、全空行不占行位、截断 XML 的末行补收）。
fn scan_sheet_slots(xml: &str, sheet: usize, sst: &SharedStrings) -> Result<Vec<CellSlot>> {
    let mut reader = Reader::from_str(xml);
    // 不裁剪文本：各分支收尾时自行 trim（与解析侧一致，实体两侧空格需保留）
    let mut buf = Vec::new();
    let mut slots: Vec<CellSlot> = Vec::new();
    let mut row_cells: Vec<RawCell> = Vec::new();
    // 行号：优先 `<row r="N">`，缺 `r` 时按出现顺序补位。空行不产出格位，
    // 但序号必须继续走（合并区 / 稀疏行的行位语义靠它）
    let mut row_seq = 0usize;
    let mut row_ref: Option<usize> = None;
    let mut cell: Option<CellAcc> = None;
    let mut in_v = false;
    let mut in_is = false;
    let mut in_is_t = false;

    loop {
        // quick-xml 0.42 的 buffer_position() 是 u64；本模块 span 统一 usize
        let pos_before = reader.buffer_position() as usize;
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name().local_name().as_ref() {
                tags::ROW => row_ref = row_number(e),
                tags::C => {
                    let mut r_attr: Option<String> = None;
                    let mut s_attr: Option<String> = None;
                    let mut ty = 's';
                    for attr in e.attributes().filter_map(|a| a.ok()) {
                        match attr.key.as_ref() {
                            "r" => r_attr = Some(attr.value.to_string()),
                            "t" => {
                                ty = match attr.value.as_ref() {
                                    "s" => 's',
                                    "n" => 'n',
                                    "b" => 'b',
                                    "e" => 'e',
                                    "str" | "inlineStr" => 'i',
                                    _ => 's',
                                };
                            }
                            "s" => s_attr = Some(attr.value.to_string()),
                            _ => {}
                        }
                    }
                    cell = Some(CellAcc {
                        start: pos_before,
                        r_attr,
                        s_attr,
                        ty,
                        value: String::new(),
                    });
                }
                tags::V => in_v = true,
                tags::IS => in_is = true,
                tags::T if in_is => in_is_t = true,
                _ => {}
            },
            Ok(Event::End(ref e)) => match e.name().local_name().as_ref() {
                tags::C => {
                    if let Some(acc) = cell.take() {
                        // `<c>` 缺 `r` 引用时按 Excel 规范「紧跟上一格」定列
                        // （解析侧同款：文档序上一格，不是排序后的）
                        let col = match acc.r_attr.as_deref() {
                            Some(r) if !r.is_empty() => col_from_ref(r),
                            _ => row_cells.last().map_or(0, |c| c.col + 1),
                        };
                        let writable = matches!(acc.ty, 's' | 'i');
                        let text = match acc.ty {
                            's' => {
                                let idx: usize = acc.value.trim().parse().unwrap_or(0);
                                sst.get(idx).unwrap_or("").to_string()
                            }
                            // 内联串与数值/布尔/错误格的 XML 形态：trim 后即文本
                            // （数值格只作占位——解析侧是格式化串，守卫天然不等）
                            _ => acc.value.trim().to_string(),
                        };
                        row_cells.push(RawCell {
                            col,
                            text,
                            span: (acc.start, reader.buffer_position() as usize),
                            r_attr: acc.r_attr,
                            s_attr: acc.s_attr,
                            writable,
                        });
                    }
                    in_v = false;
                    in_is = false;
                    in_is_t = false;
                }
                tags::ROW => {
                    let number = row_ref.take().unwrap_or(row_seq);
                    row_seq = number.saturating_add(1);
                    flush_row(&mut slots, sheet, &mut row_cells, number);
                }
                tags::V => in_v = false,
                tags::IS => in_is = false,
                tags::T if in_is_t => in_is_t = false,
                _ => {}
            },
            Ok(Event::Empty(ref e)) => {
                // `<row r="5"/>`：空行不产出格位，但行序号要跟着走（解析侧同款）；
                // 自闭合 `<c r="A2"/>`：解析侧不产格，此处不占列（镜像）
                if e.name().local_name().as_ref() == tags::ROW {
                    if let Some(n) = row_number(e) {
                        row_seq = n.saturating_add(1);
                    }
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_v || in_is_t {
                    if let Some(acc) = cell.as_mut() {
                        // quick-xml 0.42：Text 已解码，实体需显式 unescape
                        if let Ok(t) = quick_xml::escape::unescape(&e.xml10_content()) {
                            acc.value.push_str(&t);
                        }
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if in_v || in_is_t {
                    if let Some(acc) = cell.as_mut() {
                        crate::backends::office::push_general_ref(&mut acc.value, e);
                    }
                }
            }
            Ok(Event::CData(ref e)) => {
                if in_v || in_is_t {
                    if let Some(acc) = cell.as_mut() {
                        acc.value.push_str(e.as_ref());
                    }
                }
            }
            Ok(Event::Eof) => {
                // 收尾：XML 被截断时最后一行可能没有 `</row>`（解析侧同款补一次）
                let number = row_ref.take().unwrap_or(row_seq);
                flush_row(&mut slots, sheet, &mut row_cells, number);
                break;
            }
            Err(e) => return Err(DocError::corrupt(format!("sheet slot scan error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(slots)
}

/// 行收尾：列号升序排稳 → 全空行整体丢弃（不占行位，解析侧同款）→ 非空格发
/// 格位（空文本格不发——与提取侧单元格展开「trim 空即跳过」同口径——但已占
/// 列位，其后格的列下标跳位）。`number` 是该行的 0-based 物理行号（缺 `r` 的
/// 格换算 A1 引用时用）。
fn flush_row(slots: &mut Vec<CellSlot>, sheet: usize, row_cells: &mut Vec<RawCell>, number: usize) {
    row_cells.sort_by_key(|c| c.col);
    if row_cells.iter().any(|c| !c.text.trim().is_empty()) {
        for cell in row_cells.iter() {
            if cell.text.trim().is_empty() {
                continue;
            }
            let a1 = match cell.r_attr.as_deref() {
                Some(r) if !r.is_empty() => r.to_string(),
                _ => format!("{}{}", col_to_ref(cell.col), number + 1),
            };
            slots.push(CellSlot {
                sheet,
                span: cell.span,
                s_attr: cell.s_attr.clone(),
                text: cell.text.clone(),
                writable: cell.writable,
                a1,
            });
        }
    }
    row_cells.clear();
}

/// `<row r="N">` → 0-based 行号（`N - 1`）；缺 `r` 或解析失败 → `None`。
fn row_number(e: &quick_xml::events::BytesStart<'_>) -> Option<usize> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.as_ref() == "r")
        .and_then(|a| a.value.parse::<usize>().ok())
        .and_then(|n| n.checked_sub(1))
}

/// A1 引用列字母 → 0-based 列号（解析侧同款：`A1`→0、`Z1`→25、`AA1`→26）。
fn col_from_ref(r: &str) -> usize {
    let letters: String = r.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let mut col = 0usize;
    for c in letters.chars() {
        col = col * 26 + (c.to_ascii_uppercase() as u8 - b'A' + 1) as usize;
    }
    if col > 0 {
        col - 1
    } else {
        0
    }
}

/// 0-based 列号 → A1 列字母（含多字母列：`AA`、`AZ`、`ZZ`…；缺 `r` 引用的格
/// 与附录 sheet 写表头 / 数据行都用它）。
fn col_to_ref(col: usize) -> String {
    let mut n = col;
    let mut bytes = Vec::new();
    loop {
        bytes.push((n % 26) as u8 + b'A');
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    bytes.reverse();
    String::from_utf8(bytes).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 共享字符串表 / workbook sheet 清单（解析侧私有实现的回写侧镜像）
// ---------------------------------------------------------------------------

/// 共享字符串表（`xl/sharedStrings.xml`）：`<si>` 文本 trim、实体 / CDATA 解码，
/// 与解析侧 `SharedStrings` / `parse_sst_items` 同款（私有实现，此处镜像）。
struct SharedStrings {
    items: Vec<String>,
}

impl SharedStrings {
    fn load(zip: &mut zip::ZipArchive<std::fs::File>) -> Result<Self> {
        let items = match zip_read_str(zip, "xl/sharedStrings.xml")? {
            Some(xml) => parse_sst_items(&xml)?,
            None => Vec::new(),
        };
        Ok(SharedStrings { items })
    }

    fn get(&self, idx: usize) -> Option<&str> {
        self.items.get(idx).map(|s| s.as_str())
    }
}

/// 解析 `<sst>` 中的 `<si>` 条目（与解析侧 `parse_sst_items` 同款）。
fn parse_sst_items(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    // 不裁剪文本：`<si>` 收尾时 trim（实体两侧空格需保留）
    let mut buf = Vec::new();
    let mut items = Vec::new();
    let mut in_t = false;
    let mut text = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name().local_name().as_ref() {
                tags::SI => text.clear(),
                tags::T => in_t = true,
                _ => {}
            },
            Ok(Event::End(ref e)) => match e.name().local_name().as_ref() {
                tags::SI => {
                    items.push(text.trim().to_string());
                    text.clear();
                }
                tags::T => in_t = false,
                tags::SST => break,
                _ => {}
            },
            Ok(Event::Text(ref e)) => {
                if in_t {
                    if let Ok(t) = quick_xml::escape::unescape(&e.xml10_content()) {
                        text.push_str(&t);
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if in_t {
                    crate::backends::office::push_general_ref(&mut text, e);
                }
            }
            Ok(Event::CData(ref e)) => {
                if in_t {
                    text.push_str(e.as_ref());
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("SST parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(items)
}

/// workbook.xml → `[(r:id, sheet 名)]`，保持文档序（与解析侧 `parse_sheet_names`
/// 同款：属性按完整 QName 比较，`r:id` 与裸 `id` 区分）。
fn sheet_entries(xml: &str) -> Result<Vec<(String, String)>> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut sheets = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().local_name().as_ref() == "sheet" {
                    let mut rid = String::new();
                    let mut name = String::new();
                    for attr in e.attributes().filter_map(|a| a.ok()) {
                        match attr.key.as_ref() {
                            "r:id" => rid = attr.value.to_string(),
                            "name" => name = attr.value.to_string(),
                            "id" if rid.is_empty() => rid = attr.value.to_string(),
                            _ => {}
                        }
                    }
                    if !name.is_empty() {
                        sheets.push((rid, name));
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().local_name().as_ref() == "workbook" {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("workbook parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(sheets)
}

// ---------------------------------------------------------------------------
// 配对与改写
// ---------------------------------------------------------------------------

/// 格位 × 翻译单元按序配对（文本一致性守卫）+ 译文查找。
/// 返回 (slot 下标, 译文) 列表，文档序。
fn matched_translations(
    slots: &[CellSlot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, &str>,
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let n = slots.len().min(units.len());
    for k in 0..n {
        // 双保险：格位文本与单元文本必须一致（同一解析器同一文件的确定性保证）
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

/// 目标格整格替换为 inlineStr：`r`（定位，缺 `r` 的格按行列换算补名）与 `s`
/// （样式）保留，`t` 换 `"inlineStr"`、内容换 `<is><t>…</t></is>`——影响局部于
/// 该 `<c>`，同串多引用的其他格与 sharedStrings.xml 均不受扰（见模块头
/// 「为什么写 inlineStr」）。
fn inline_str_cell(slot: &CellSlot, text: &str) -> String {
    let mut attrs = format!(r#" r="{}""#, slot.a1);
    if let Some(s) = &slot.s_attr {
        attrs.push_str(&format!(r#" s="{s}""#));
    }
    format!(r#"<c{attrs} t="inlineStr"><is><t>{}</t></is></c>"#, escape_xml_text(text))
}

// ---------------------------------------------------------------------------
// OriginalPlusAppendix：追加「译文」sheet
// ---------------------------------------------------------------------------

/// sheet 名 → `sheet!A1` 引用：按 Excel 公式引用规则，名字含字母/数字/下划线
/// 之外字符、不以字母开头、或形如单元格引用（如 `S1`、`A1`）时加单引号。
fn sheet_cell_ref(name: &str, a1: &str) -> String {
    let plain = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let first_alpha = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    let letters = name.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    let cell_like = (1..=3).contains(&letters)
        && letters < name.chars().count()
        && name.chars().skip(letters).all(|c| c.is_ascii_digit());
    if plain && first_alpha && !cell_like {
        format!("{name}!{a1}")
    } else {
        format!("'{}'!{a1}", name.replace('\'', "''"))
    }
}

/// 附录 sheet XML：表头行 `单元格 / 原文 / 译文` + 数据行，全部 inlineStr
/// （不引 sharedStrings，与正文改写同一套零副作用口径）。
fn appendix_sheet_xml(rows: &[(String, String, String)]) -> String {
    let mut body = String::from(
        r#"<row r="1"><c r="A1" t="inlineStr"><is><t>单元格</t></is></c>"#,
    );
    body.push_str(r#"<c r="B1" t="inlineStr"><is><t>原文</t></is></c>"#);
    body.push_str(r#"<c r="C1" t="inlineStr"><is><t>译文</t></is></c></row>"#);
    for (i, (cell, orig, tr)) in rows.iter().enumerate() {
        let r = i + 2;
        body.push_str(&format!(
            concat!(
                r#"<row r="{r}"><c r="A{r}" t="inlineStr"><is><t>{cell}</t></is></c>"#,
                r#"<c r="B{r}" t="inlineStr"><is><t>{orig}</t></is></c>"#,
                r#"<c r="C{r}" t="inlineStr"><is><t>{tr}</t></is></c></row>"#
            ),
            r = r,
            cell = escape_xml_text(cell),
            orig = escape_xml_text(orig),
            tr = escape_xml_text(tr),
        ));
    }
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
            "<sheetData>{body}</sheetData></worksheet>"
        ),
        body = body
    )
}

/// 组装「译文」附录：新 worksheet 部件 + workbook 声明 + rels 关系 +
/// `[Content_Types].xml` override。返回 (要替换的部件, 要新增的部件)。
/// 部件名取既有 `xl/worksheets/sheetN.xml` 最大序号 + 1；sheet 名与既有名冲突
/// 时依次退让（`译文`、`译文2`…）；workbook / rels 结构坏到接不上声明时报
/// `corrupt`（宁失败不错包）。
#[allow(clippy::too_many_arguments)]
fn build_appendix(
    entry_names: &[String],
    wb_xml: &str,
    rels_xml: Option<&str>,
    ct_xml: Option<&str>,
    sheet_names: &[String],
    rows: &[(String, String, String)],
) -> Result<(ZipParts, ZipParts)> {
    // sheet 名冲突防御：`译文`、`译文2`、`译文3`… 依次取第一个空位
    let mut name = "译文".to_string();
    let mut n = 1usize;
    while sheet_names.iter().any(|s| s == &name) {
        n += 1;
        name = format!("译文{n}");
    }

    // 新部件名：既有 `xl/worksheets/sheetN.xml` 的最大序号 + 1（只认纯数字
    // 序号部件，撞不上任何既有条目）
    let max_part = entry_names
        .iter()
        .filter_map(|n| {
            n.strip_prefix("xl/worksheets/sheet")
                .and_then(|r| r.strip_suffix(".xml"))
                .and_then(|m| m.parse::<usize>().ok())
        })
        .max()
        .unwrap_or(0);
    let part = format!("xl/worksheets/sheet{}.xml", max_part + 1);
    let part_base = part.rsplit('/').next().unwrap_or(&part).to_string();

    // workbook.xml：`</sheets>` 前插 `<sheet>` 声明（缺 `</sheets>` 退到
    // `</workbook>` 前；两处都没有 = 包坏，报 corrupt）
    let sheet_id = max_attr_usize(wb_xml, "sheet", "sheetId").map_or(1, |m| m + 1);
    let rid = rels_xml
        .map(rel_entries)
        .unwrap_or_default()
        .iter()
        .filter_map(|(id, _, _)| id.strip_prefix("rId").and_then(|v| v.parse::<usize>().ok()))
        .max()
        .map_or(1, |m| m + 1);
    let decl = format!(r#"<sheet name="{name}" sheetId="{sheet_id}" r:id="rId{rid}"/>"#);
    let new_wb = match wb_xml.rfind("</sheets>") {
        Some(p) => insert_at(wb_xml, p, &decl),
        None => match wb_xml.rfind("</workbook>") {
            Some(p) => insert_at(wb_xml, p, &decl),
            None => {
                return Err(DocError::corrupt(
                    "workbook.xml 缺 </sheets>，无法追加附录 sheet",
                ))
            }
        },
    };

    // workbook rels：追加 worksheet 关系；rels 缺失则整件新建（既有 sheet 的
    // 部件解析按 r:id 查不到才回退序号命名，不受新增关系影响）
    let rel = format!(
        concat!(
            r#"<Relationship Id="rId{rid}" "#,
            r#"Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" "#,
            r#"Target="worksheets/{base}"/>"#
        ),
        rid = rid,
        base = part_base
    );
    let mut replaced: Vec<(String, Vec<u8>)> =
        vec![("xl/workbook.xml".to_string(), new_wb.into_bytes())];
    let mut added: Vec<(String, Vec<u8>)> =
        vec![(part.clone(), appendix_sheet_xml(rows).into_bytes())];
    match rels_xml {
        Some(xml) => match xml.rfind("</Relationships>") {
            Some(p) => replaced.push((
                "xl/_rels/workbook.xml.rels".to_string(),
                insert_at(xml, p, &rel).into_bytes(),
            )),
            None => {
                return Err(DocError::corrupt(
                    "workbook rels 缺 </Relationships>，无法追加附录 sheet",
                ))
            }
        },
        None => added.push((
            "xl/_rels/workbook.xml.rels".to_string(),
            format!(
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rel}</Relationships>"#
            )
            .into_bytes(),
        )),
    }

    // [Content_Types].xml：追加 override；缺失则整件新建，坏到没有 `</Types>`
    // 时不动（结构修复不在回写职责内）
    const SHEET_CT: &str =
        "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml";
    let override_el = format!(r#"<Override PartName="/{part}" ContentType="{SHEET_CT}"/>"#);
    match ct_xml {
        Some(xml) => {
            if let Some(p) = xml.rfind("</Types>") {
                replaced.push((
                    "[Content_Types].xml".to_string(),
                    insert_at(xml, p, &override_el).into_bytes(),
                ));
            }
        }
        None => added.push((
            "[Content_Types].xml".to_string(),
            format!(
                r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">{override_el}</Types>"#
            )
            .into_bytes(),
        )),
    }
    Ok((replaced, added))
}

/// 扫 XML 里指定元素的某数字属性最大值（workbook `sheetId` 之类；坏值忽略）。
fn max_attr_usize(xml: &str, elem: &str, attr: &str) -> Option<usize> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut max: Option<usize> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().local_name().as_ref() == elem {
                    for a in e.attributes().filter_map(|a| a.ok()) {
                        if a.key.as_ref() == attr {
                            if let Ok(v) = a.value.parse::<usize>() {
                                max = Some(max.map_or(v, |m| m.max(v)));
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    max
}

/// 在 `pos` 前插入片段（其余字节原样）。
fn insert_at(xml: &str, pos: usize, piece: &str) -> String {
    let mut out = String::with_capacity(xml.len() + piece.len());
    out.push_str(&xml[..pos]);
    out.push_str(piece);
    out.push_str(&xml[pos..]);
    out
}

// ---------------------------------------------------------------------------
// ZIP 透传重打包（xlsx 专用：支持新增条目——「译文」附录 sheet 是新部件；
// writeback::zip_util 的 rewrite_zip 只支持替换既有条目，xlsx-only 构建下也
// 不参与那套 cfg 门控）
// ---------------------------------------------------------------------------

/// 一批 ZIP 部件（条目名 → 字节）：替换 / 新增清单共用此形。
type ZipParts = Vec<(String, Vec<u8>)>;

/// 逐条目透传原包字节，`replaced` 中的条目换成新字节，末尾追加 `added` 新条目，
/// 重打包到 `out`。
fn rewrite_zip_with_added(
    in_path: &Path,
    out_path: &Path,
    replaced: &[(String, Vec<u8>)],
    added: &[(String, Vec<u8>)],
) -> Result<()> {
    use std::io::{Read as _, Write as _};

    let mut zip = open_zip(in_path)?;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip
            .by_index(i)
            .map_err(|e| DocError::corrupt(format!("ZIP 读取条目 {i} 失败: {e}")))?;
        if f.is_dir() {
            continue;
        }
        let name = f
            .name()
            .map_err(|e| DocError::corrupt(format!("ZIP 条目 {i} 名字损坏: {e}")))?
            .into_owned();
        let mut data = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut data)
            .map_err(|e| DocError::corrupt(format!("ZIP 读取 {name} 失败: {e}")))?;
        entries.push((name, data));
    }
    // 追加条目不得与既有条目撞名（sheetN 由既有序号 +1 推得，理论上不可能）
    for (name, _) in added {
        if entries.iter().any(|(n, _)| n == name) {
            return Err(DocError::corrupt(format!("追加条目 {name} 与既有部件撞名")));
        }
    }

    let out_file = std::fs::File::create(out_path).map_err(|e| {
        DocError::corrupt(format!("cannot create {}: {e}", out_path.display()))
    })?;
    let mut zw = zip::ZipWriter::new(out_file);
    let opts = zip::write::SimpleFileOptions::default();
    for (name, mut data) in entries {
        if let Some((_, new_data)) = replaced.iter().find(|(n, _)| *n == name) {
            data = new_data.clone();
        }
        zw.start_file(name.as_str(), opts)
            .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
        zw.write_all(&data)
            .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
    }
    for (name, data) in added {
        zw.start_file(name.as_str(), opts)
            .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
        zw.write_all(data)
            .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
    }
    zw.finish()
        .map_err(|e| DocError::corrupt(format!("ZIP 收尾失败: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::path::PathBuf;

    use crate::emit::to_translation_units;
    use crate::model::BlockKind;

    const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

    /// 单 sheet（S1）工作簿骨架。
    const WB_XML: &str = concat!(
        r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" "#,
        r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
        r#"<sheets><sheet name="S1" sheetId="1" r:id="rId1"/></sheets></workbook>"#
    );
    const WB_RELS: &str = concat!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
        r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>"#,
        r#"</Relationships>"#
    );
    /// xf0 = General、xf1 = 内置日期格式 14（`s="1"` 的日期格引用）。
    const STYLES_XML: &str = concat!(
        r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
        r#"<cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="14"/></cellXfs></styleSheet>"#
    );
    const SST_XML: &str = concat!(
        r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
        r#"<si><t>名称</t></si><si><t>数量</t></si></sst>"#
    );
    const CT_XML: &str = concat!(
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
        r#"<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>"#,
        r#"<Default Extension="xml" ContentType="application/xml"/>"#,
        r#"<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>"#,
        r#"</Types>"#
    );

    fn worksheet(rows: &str, extra: &str) -> String {
        format!(
            r#"<worksheet xmlns="{MAIN_NS}"><sheetData>{rows}</sheetData>{extra}</worksheet>"#
        )
    }

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_xlsx_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_xlsx(path: &Path, parts: &[(&str, String)]) {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in parts {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        zw.finish().unwrap();
    }

    /// 与 writeback 同一套装配（= 提取侧默认装配）。
    fn parse_doc(path: &Path) -> crate::model::Document {
        let raw = XlsxRawBackend::default().parse(path).unwrap();
        OfficePlugin::default().assemble(&raw)
    }

    fn table_rows(path: &Path, page: usize) -> Vec<Vec<String>> {
        parse_doc(path).pages[page].blocks[0]
            .table
            .as_ref()
            .unwrap()
            .rows
            .clone()
    }

    fn write(in_path: &Path, out_path: &Path, tr: &[(&str, &str)], strategy: BiStrategy) {
        let translated: Vec<(String, String)> =
            tr.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        writeback_xlsx(in_path, &translated, strategy, out_path).unwrap();
    }

    fn entry_bytes(zip_path: &Path, name: &str) -> Vec<u8> {
        let mut z = open_zip(zip_path).unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut z.by_name(name).unwrap(), &mut buf).unwrap();
        buf
    }

    fn entry_str(zip_path: &Path, name: &str) -> String {
        String::from_utf8(entry_bytes(zip_path, name)).unwrap()
    }

    /// 金测（核心验收）：TranslationOnly 往返 —— 共享串 / 内联串格全译替换，
    /// 数字格与日期格被「可写判据 + 文本守卫」双重拦下原样不动，且
    /// sharedStrings.xml 字节不变（inlineStr 化：改写局部于 `<c>`）。
    #[test]
    fn translation_only_roundtrip_guards_numbers_and_shared_strings() {
        let dir = case_dir("tonly");
        let in_path = dir.join("in.xlsx");
        let sheet1 = worksheet(
            concat!(
                r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>"#,
                r#"<row r="2"><c r="A2" t="inlineStr"><is><t>华东</t></is></c><c r="B2" t="n"><v>220</v></c></row>"#,
                r#"<row r="3"><c r="A3" s="1" t="n"><v>45292</v></c><c r="B3" t="inlineStr"><is><t>备注</t></is></c></row>"#,
            ),
            "",
        );
        build_xlsx(
            &in_path,
            &[
                ("[Content_Types].xml", CT_XML.to_string()),
                ("xl/workbook.xml", WB_XML.to_string()),
                ("xl/_rels/workbook.xml.rels", WB_RELS.to_string()),
                ("xl/sharedStrings.xml", SST_XML.to_string()),
                ("xl/styles.xml", STYLES_XML.to_string()),
                ("xl/worksheets/sheet1.xml", sheet1),
            ],
        );

        // 单元流：格阈值恒 1，数字/日期格也产单元（解析侧有格式化文本）
        let units = to_translation_units(&parse_doc(&in_path));
        assert_eq!(
            units.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            [
                "pS1-b0-r0c0",
                "pS1-b0-r0c1",
                "pS1-b0-r1c0",
                "pS1-b0-r1c1",
                "pS1-b0-r2c0",
                "pS1-b0-r2c1",
            ]
        );
        // 文本口径：共享串解引用、内联串、数字规整、日期格式化各就各位
        assert_eq!(units[0].text, "名称");
        assert_eq!(units[1].text, "数量");
        assert_eq!(units[2].text, "华东");
        assert_eq!(units[3].text, "220");
        let date_text = units[4].text.clone();
        assert!(date_text.contains("24"), "日期格按解析侧格式化：{date_text}");
        assert_eq!(units[5].text, "备注");

        // 格单元全译（含数字/日期格——回写侧应守卫跳过这两类）
        let translated: Vec<(String, String)> = units
            .iter()
            .map(|u| (u.id.clone(), format!("[{}]", u.text)))
            .collect();
        let out_path = dir.join("out.xlsx");
        writeback_xlsx(&in_path, &translated, BiStrategy::TranslationOnly, &out_path).unwrap();

        // 重解析：命中字符格 == 译文；数字/日期格原样
        let got = table_rows(&out_path, 0);
        assert_eq!(
            got,
            vec![
                vec!["[名称]".to_string(), "[数量]".to_string()],
                vec!["[华东]".to_string(), "220".to_string()],
                vec![date_text, "[备注]".to_string()],
            ]
        );

        // sharedStrings.xml 字节不变（inlineStr 化断言）+ 数字/日期格 XML 原样
        assert_eq!(
            entry_bytes(&in_path, "xl/sharedStrings.xml"),
            entry_bytes(&out_path, "xl/sharedStrings.xml")
        );
        let out_sheet = entry_str(&out_path, "xl/worksheets/sheet1.xml");
        assert!(out_sheet.contains(r#"<c r="B2" t="n"><v>220</v></c>"#), "{out_sheet}");
        assert!(out_sheet.contains(r#"<c r="A3" s="1" t="n"><v>45292</v></c>"#));
        assert!(out_sheet.contains(r#"<c r="A1" t="inlineStr"><is><t>[名称]</t></is></c>"#));
        // 样式属性保留（s="1" 跟着译文走）
        assert!(out_sheet.contains(r#"t="inlineStr"><is><t>[备注]</t></is></c>"#));
        // 未触及部件字节透传
        assert_eq!(entry_bytes(&in_path, "xl/styles.xml"), entry_bytes(&out_path, "xl/styles.xml"));
        assert_eq!(
            entry_bytes(&in_path, "[Content_Types].xml"),
            entry_bytes(&out_path, "[Content_Types].xml")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ref 换算：AA 列及以后 + used-range 非自 A1 起（跳行）+ 缺 `r` 引用的格
    /// （列 = 上一格 +1、行 = 行序号 +1 换算出引用落名）。
    #[test]
    fn ref_conversion_beyond_z_and_offset_used_range() {
        let dir = case_dir("ref");
        let in_path = dir.join("in.xlsx");
        let sheet1 = worksheet(
            concat!(
                r#"<row r="2"><c r="Z2" t="inlineStr"><is><t>廿六</t></is></c><c r="AA2" t="inlineStr"><is><t>廿七</t></is></c></row>"#,
                r#"<row r="4"/>"#,
                r#"<row r="5"><c r="C5" t="inlineStr"><is><t>深处</t></is></c></row>"#,
                r#"<row r="6"><c t="inlineStr"><is><t>无引用</t></is></c><c t="inlineStr"><is><t>次列</t></is></c></row>"#,
            ),
            "",
        );
        build_xlsx(
            &in_path,
            &[
                ("xl/workbook.xml", WB_XML.to_string()),
                ("xl/_rels/workbook.xml.rels", WB_RELS.to_string()),
                ("xl/worksheets/sheet1.xml", sheet1),
            ],
        );

        // 空行 r=4 不占行位：深处 = r1；缺引用格补列：无引用 = r2c0、次列 = r2c1
        let units = to_translation_units(&parse_doc(&in_path));
        assert_eq!(
            units.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            ["pS1-b0-r0c0", "pS1-b0-r0c1", "pS1-b0-r1c0", "pS1-b0-r2c0", "pS1-b0-r2c1"]
        );
        let translated: Vec<(String, String)> = units
            .iter()
            .map(|u| (u.id.clone(), format!("[{}]", u.text)))
            .collect();
        let out_path = dir.join("out.xlsx");
        writeback_xlsx(&in_path, &translated, BiStrategy::TranslationOnly, &out_path).unwrap();

        // 多字母列与偏移行的原引用原样落位；缺引用格按换算 ref 补名
        let out_sheet = entry_str(&out_path, "xl/worksheets/sheet1.xml");
        assert!(out_sheet.contains(r#"<c r="Z2" t="inlineStr"><is><t>[廿六]</t></is></c>"#), "{out_sheet}");
        assert!(out_sheet.contains(r#"<c r="AA2" t="inlineStr"><is><t>[廿七]</t></is></c>"#));
        assert!(out_sheet.contains(r#"<c r="C5" t="inlineStr"><is><t>[深处]</t></is></c>"#));
        assert!(out_sheet.contains(r#"<c r="A6" t="inlineStr"><is><t>[无引用]</t></is></c>"#));
        assert!(out_sheet.contains(r#"<c r="B6" t="inlineStr"><is><t>[次列]</t></is></c>"#));

        // 重解析：网格与译文都对
        assert_eq!(
            table_rows(&out_path, 0),
            vec![
                vec!["[廿六]".to_string(), "[廿七]".to_string()],
                vec!["[深处]".to_string()],
                vec!["[无引用]".to_string(), "[次列]".to_string()],
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BilingualInline：格内 `原文\n译文`（同 inlineStr 写回，换行进 `<t>`）。
    #[test]
    fn bilingual_inline_joins_translation_in_cell() {
        let dir = case_dir("inline");
        let in_path = dir.join("in.xlsx");
        let sheet1 = worksheet(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>甲</t></is></c><c r="B1" t="inlineStr"><is><t>乙</t></is></c></row>"#,
            "",
        );
        build_xlsx(
            &in_path,
            &[
                ("xl/workbook.xml", WB_XML.to_string()),
                ("xl/_rels/workbook.xml.rels", WB_RELS.to_string()),
                ("xl/worksheets/sheet1.xml", sheet1),
            ],
        );

        let out_path = dir.join("out.xlsx");
        write(
            &in_path,
            &out_path,
            &[("pS1-b0-r0c0", "Alpha"), ("pS1-b0-r0c1", "Beta")],
            BiStrategy::BilingualInline,
        );

        // 格文本 = 原文\n译文（提取侧格内换行同口径）；XML 层是单 `<t>` 带换行
        assert_eq!(
            table_rows(&out_path, 0),
            vec![vec![
                "甲\nAlpha".to_string(),
                "乙\nBeta".to_string(),
            ]]
        );
        let out_sheet = entry_str(&out_path, "xl/worksheets/sheet1.xml");
        assert!(out_sheet.contains("<is><t>甲\nAlpha</t></is>"), "{out_sheet}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：正文格原样，追加「译文」sheet（表头 + 单元格 ref /
    /// 原文 / 译文），workbook 声明 / rels / content-types 三处接线，可重解析。
    #[test]
    fn appendix_appends_translated_sheet() {
        let dir = case_dir("appendix");
        let in_path = dir.join("in.xlsx");
        let sheet1 = worksheet(
            concat!(
                r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>"#,
                r#"<row r="2"><c r="A2" t="inlineStr"><is><t>华东</t></is></c><c r="B2" t="n"><v>220</v></c></row>"#,
                r#"<row r="3"><c r="A3" s="1" t="n"><v>45292</v></c><c r="B3" t="inlineStr"><is><t>备注</t></is></c></row>"#,
            ),
            "",
        );
        build_xlsx(
            &in_path,
            &[
                ("[Content_Types].xml", CT_XML.to_string()),
                ("xl/workbook.xml", WB_XML.to_string()),
                ("xl/_rels/workbook.xml.rels", WB_RELS.to_string()),
                ("xl/sharedStrings.xml", SST_XML.to_string()),
                ("xl/styles.xml", STYLES_XML.to_string()),
                ("xl/worksheets/sheet1.xml", sheet1),
            ],
        );

        let units = to_translation_units(&parse_doc(&in_path));
        let translated: Vec<(String, String)> = units
            .iter()
            .map(|u| (u.id.clone(), format!("[{}]", u.text)))
            .collect();
        let out_path = dir.join("out.xlsx");
        writeback_xlsx(&in_path, &translated, BiStrategy::OriginalPlusAppendix, &out_path).unwrap();

        // 正文 sheet 原样；附录 sheet 可重解析
        assert_eq!(parse_doc(&out_path).pages.len(), 2);
        assert_eq!(parse_doc(&out_path).pages[0].name.as_deref(), Some("S1"));
        let orig_rows = table_rows(&in_path, 0);
        assert_eq!(table_rows(&out_path, 0), orig_rows, "正文格一律不动");

        let appendix = parse_doc(&out_path).pages[1].clone();
        assert_eq!(appendix.name.as_deref(), Some("译文"));
        let rows: Vec<Vec<String>> = appendix.blocks[0]
            .table
            .as_ref()
            .unwrap()
            .rows
            .clone();
        assert_eq!(rows[0], vec!["单元格".to_string(), "原文".to_string(), "译文".to_string()]);
        // S1 形如单元格引用 → 引用带单引号；数字格（220）不进附录（不可写）
        assert_eq!(rows[1], vec!["'S1'!A1".to_string(), "名称".to_string(), "[名称]".to_string()]);
        assert_eq!(rows[2], vec!["'S1'!B1".to_string(), "数量".to_string(), "[数量]".to_string()]);
        assert_eq!(rows[3], vec!["'S1'!A2".to_string(), "华东".to_string(), "[华东]".to_string()]);
        assert_eq!(rows[4], vec!["'S1'!B3".to_string(), "备注".to_string(), "[备注]".to_string()]);
        assert_eq!(rows.len(), 5, "表头 + 4 条可写格（数字/日期格不进附录）");

        // workbook 声明 / rels 关系 / content-types override 三处接线
        let out_wb = entry_str(&out_path, "xl/workbook.xml");
        assert!(out_wb.contains(r#"<sheet name="译文" sheetId="2" r:id="rId2"/>"#), "{out_wb}");
        let out_rels = entry_str(&out_path, "xl/_rels/workbook.xml.rels");
        assert!(out_rels.contains(r#"Id="rId2""#) && out_rels.contains(r#"Target="worksheets/sheet2.xml""#));
        let out_ct = entry_str(&out_path, "[Content_Types].xml");
        assert!(out_ct.contains(r#"PartName="/xl/worksheets/sheet2.xml""#));
        // 原 sheet1.xml 字节不动
        assert_eq!(
            entry_bytes(&in_path, "xl/worksheets/sheet1.xml"),
            entry_bytes(&out_path, "xl/worksheets/sheet1.xml")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 附录 sheet 名冲突防御：已有名为「译文」的 sheet → 退让为「译文2」。
    #[test]
    fn appendix_sheet_name_conflict_defense() {
        let dir = case_dir("appendix_conflict");
        let in_path = dir.join("in.xlsx");
        let wb = concat!(
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<sheets><sheet name="S1" sheetId="1" r:id="rId1"/>"#,
            r#"<sheet name="译文" sheetId="2" r:id="rId2"/></sheets></workbook>"#
        );
        let rels = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>"#,
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>"#,
            r#"</Relationships>"#
        );
        let sheet1 = worksheet(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>甲</t></is></c></row>"#,
            "",
        );
        let sheet2 = worksheet(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>已有</t></is></c></row>"#,
            "",
        );
        build_xlsx(
            &in_path,
            &[
                ("xl/workbook.xml", wb.to_string()),
                ("xl/_rels/workbook.xml.rels", rels.to_string()),
                ("xl/worksheets/sheet1.xml", sheet1),
                ("xl/worksheets/sheet2.xml", sheet2),
            ],
        );

        let units = to_translation_units(&parse_doc(&in_path));
        assert_eq!(units.len(), 2);
        let translated: Vec<(String, String)> = units
            .iter()
            .map(|u| (u.id.clone(), format!("[{}]", u.text)))
            .collect();
        let out_path = dir.join("out.xlsx");
        writeback_xlsx(&in_path, &translated, BiStrategy::OriginalPlusAppendix, &out_path).unwrap();

        let doc = parse_doc(&out_path);
        assert_eq!(doc.pages.len(), 3);
        assert_eq!(doc.pages[1].name.as_deref(), Some("译文"), "既有「译文」不受扰");
        assert_eq!(doc.pages[2].name.as_deref(), Some("译文2"), "冲突退让 +1");
        // 新部件接在既有最大序号（sheet2）之后 = sheet3
        let out_wb = entry_str(&out_path, "xl/workbook.xml");
        assert!(out_wb.contains(r#"<sheet name="译文2" sheetId="3" r:id="rId3"/>"#), "{out_wb}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// merges 表 + 空白译文 + 未匹配 id：合并展开位不误写（自闭合 `<c r="B1"/>`
    /// 字节原样）、空白译文视同未提供、未匹配 id 忽略；merges 元信息透传。
    #[test]
    fn merges_blank_and_unmatched_leave_sheet_intact() {
        let dir = case_dir("merges");
        let in_path = dir.join("in.xlsx");
        let sheet1 = worksheet(
            concat!(
                r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"/><c r="C1"/></row>"#,
                r#"<row r="2"><c r="A2" t="inlineStr"><is><t>甲</t></is></c></row>"#,
            ),
            r#"<mergeCells count="1"><mergeCell ref="A1:C1"/></mergeCells>"#,
        );
        let sst = concat!(
            r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
            r#"<si><t>标题</t></si></sst>"#
        );
        build_xlsx(
            &in_path,
            &[
                ("xl/workbook.xml", WB_XML.to_string()),
                ("xl/_rels/workbook.xml.rels", WB_RELS.to_string()),
                ("xl/sharedStrings.xml", sst.to_string()),
                ("xl/worksheets/sheet1.xml", sheet1),
            ],
        );

        // 自闭合空格（B1/C1）不产格：单元流只有标题与甲
        let units = to_translation_units(&parse_doc(&in_path));
        assert_eq!(
            units.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            ["pS1-b0-r0c0", "pS1-b0-r1c0"]
        );

        let out_path = dir.join("out.xlsx");
        write(
            &in_path,
            &out_path,
            &[
                ("pS1-b0-r0c0", "[标题]"),
                ("pS1-b0-r1c0", "   "), // 空白译文：视同未提供
                ("pX-b99", "bogus"),    // 未匹配 id：忽略
            ],
            BiStrategy::TranslationOnly,
        );

        let got = parse_doc(&out_path).pages[0].blocks[0].clone();
        assert_eq!(
            got.table.as_ref().unwrap().rows,
            vec![vec!["[标题]".to_string()], vec!["甲".to_string()]]
        );
        assert_eq!(
            got.meta.get("merges"),
            Some(&crate::model::MetaValue::List(vec!["A1:C1".to_string()])),
            "merges 元信息透传"
        );
        // 合并展开位（自闭合空格）与未译格字节原样
        let out_sheet = entry_str(&out_path, "xl/worksheets/sheet1.xml");
        assert!(out_sheet.contains(r#"<c r="B1"/>"#), "{out_sheet}");
        assert!(out_sheet.contains(r#"<c r="C1"/>"#));
        assert!(out_sheet.contains(r#"<is><t>甲</t></is>"#));
        assert!(out_sheet.contains(r#"<c r="A1" t="inlineStr"><is><t>[标题]</t></is></c>"#));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全部 id 未匹配：输出与原包逐条目字节一致（透传）。
    #[test]
    fn unmatched_ids_leave_workbook_untouched() {
        let dir = case_dir("unmatched");
        let in_path = dir.join("in.xlsx");
        let sheet1 = worksheet(
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>甲</t></is></c></row>"#,
            "",
        );
        build_xlsx(
            &in_path,
            &[
                ("xl/workbook.xml", WB_XML.to_string()),
                ("xl/_rels/workbook.xml.rels", WB_RELS.to_string()),
                ("xl/worksheets/sheet1.xml", sheet1),
            ],
        );
        let out_path = dir.join("out.xlsx");
        write(&in_path, &out_path, &[("p0-b42", "nope")], BiStrategy::BilingualInline);
        assert_eq!(table_rows(&out_path, 0), table_rows(&in_path, 0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文本守卫（格位同款）：格文本 ≠ 单元文本 → 不命中（宁漏勿错）；
    /// 位置配对是权威，文本守卫只把关写不写。
    #[test]
    fn text_guard_skips_mismatched_slot() {
        let unit = |id: &str, text: &str| crate::emit::TranslationUnit {
            id: id.to_string(),
            page: Some("S1".to_string()),
            kind: BlockKind::Table,
            text: text.to_string(),
            source: crate::model::BlockSource::Native,
            confidence: None,
            lang: None,
            chapter: None,
            header: false,
            running: false,
        };
        let entries = [("pS1-b0-r0c0".to_string(), "格译文".to_string())];
        let map = translation_map(&entries);

        let slots = [CellSlot {
            sheet: 0,
            span: (0, 0),
            s_attr: None,
            text: "格文本".to_string(),
            writable: true,
            a1: "A1".to_string(),
        }];
        assert!(
            matched_translations(&slots, &[unit("pS1-b0-r0c0", "单元文本")], &map).is_empty(),
            "文本不一致的格不写"
        );

        // 反例：文本一致时照常命中
        let got = matched_translations(&slots, &[unit("pS1-b0-r0c0", "格文本")], &map);
        assert_eq!(got, vec![(0, "格译文".to_string())]);
    }

    /// ref 换算双向：`AA`/`AZ`/`ZZ` 多字母列与解析侧 `col_from_ref` 互逆。
    #[test]
    fn col_ref_roundtrip() {
        for (col, want) in [(0usize, "A"), (25, "Z"), (26, "AA"), (27, "AB"), (51, "AZ"), (701, "ZZ")] {
            assert_eq!(col_to_ref(col), want);
            assert_eq!(col_from_ref(&format!("{want}1")), col, "{want} 往返");
        }
    }
}
