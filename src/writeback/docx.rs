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
//! * 源校验（①→④ 边界）：[`crate::emit::TranslationPair`] 的 `source` 携带
//!   提取侧源文本时，写入前与当前位文本比对，不一致跳过并记入
//!   [`WritebackReport`]——①与④两次解析之间文件被改时位置锚点会指向别的
//!   文本，守卫两侧都来自④拦不住这种漂移（静默错位写是最恶劣故障模式）；
//!   `source: None` = 信任锚点（原行为）。报告口径：written = 实际写入
//!   （含格），skipped 收源校验失败 / 文本守卫失败 / 结构跳过（嵌套表格、
//!   无文本节点、附录不含表格、单元无对应位等）。
//! * 三策略的表格口径：`TranslationOnly` 格内首 `w:t` 换译文、其余清空
//!   （多段落格的其余段一并清空——重解析格文本 = 译文，与提取侧 join 口径
//!   一致）；`BilingualInline` 格内最后一个 `w:p` 之后追加译段（复制格内
//!   首段 `w:pPr`；译文 run 带全新最小 `w:rPr`，固定小字号 `w:sz`/`w:szCs`
//!   = 18 半点 = 9pt——窄列长译文会把表格撑乱，小字号显著缓解；不复制原
//!   run 的 rPr，run 级属性本就不该跨语言复制，字体由主题默认继承；固定
//!   9pt 不解析 styles.xml 继承链（成本不成比例），对正文 10.5-12pt 是
//!   明确的"译文辅文"层级，未来可做样式感知；不用 `w:br` 软换行——会破
//!   docx 表格行高语义）；
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
//!
//! # 节交替（[`writeback_docx_alternating`]，rumi「一页原文一页译文」#12）
//!
//! Word 无真页，最近的页边界是分节符（段落 pPr 内 `w:sectPr` 标该节结束；
//! body 末尾裸 `w:sectPr` 标最后一节）与分页符（run 内 `<w:br w:type="page"/>`）。
//! 按这些边界把 body 内容切段（解析侧不识别节/分页——分段纯是回写侧 XML 层
//! 的事），每**有命中编辑**的原文段后插「该段的纯译文段」，即
//! `writeback_epub_alternating`（epub 部件交替，epub 侧同形态）在
//! docx 的对应物：命中 = 有实际落盘编辑（守卫/结构跳过不算），无命中段不插
//! 译文段（避免原样页翻两遍）。译文段 = 该段 XML 子串平移套用同一套
//! TranslationOnly 逐位置编辑（段落/格位口径不变，未命中单元原样保留——译文
//! 段是完整合法的正文拷贝，含表格等结构块），不另写渲染；原文段字节不动
//! （只插入）。
//!
//! 节保真 / 分页语义载体（两案金测后定稿，见 `assemble_alternating`）：
//! 分节切的段**不追加任何载体段**——译文段是原段字节拷贝、段内自带同一节
//! 符段落，译文节属性逐字节保真（同页向/页眉页脚，headerReference 的 rId
//! 多节共享合法），前后换页均由拷贝节符自带（默认 nextPage 起新页）。设计
//! 稿的「译文段末追加 sectPr 拷贝段」经金测分析否决：译文段内已有拷贝节符，
//! 再追加一节符段会产生只含一个空段的「空节」，渲染为译文与下一段原文之间
//! 的空白页。尾节（body 裸 sectPr）/退化段/分页符细分段中段首无分页者，译
//! 文段前插独立分页段 `PAGE_BREAK_P`（`w:br w:type="page"` 硬分页）；尾节
//! 译文随下一节属性——裸 sectPr 即本节原属性，保真等价；段首自带分页段落
//! 的细分段，译文段拷贝自带段首分页，同样不另插（否则空白页）。
//!
//! # 回写保真小项
//!
//! * BilingualInline 追加段的节符防御：原段是节尾段（pPr 内含 `w:sectPr`）
//!   时，追加段只克隆 pPr 的非节符部分（`apply_inline` / `strip_sectpr`）
//!   ——复制节符 = 译文段自带换页边界，双语段之间多出一页空白（与上文金
//!   测否决案同根因）。
//! * TranslationOnly 原地替换的 `xml:space` 条件补（`translation_only_edits`
//!   / `preserve_space_tag_edit`）：译文原文首/尾带空白且原 `w:t` 标签未带
//!   `xml:space` 时补 `preserve`——合成段落恒带 preserve（防 Word 吞空格），
//!   原地替换路径原来没有，两条写入路径口径对齐。

use std::collections::HashMap;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::backends::office::docx::DocxRawBackend;
use crate::backends::office::{tags, OfficePlugin};
use crate::emit::BiStrategy;
use crate::error::{DocError, Result};
use crate::plugin::{FormatPlugin, ParseCapability};

use super::{escape_xml_text, open_zip, pair_map, rewrite_zip, WritebackReport};

/// docx 双语回写：见模块级文档。`pairs` 的 id 与
/// [`crate::emit::to_translation_units`] 同口径（块 `p{页}-b{块序}`、表格格
/// `p{页}-b{块序}-r{行}c{列}`）；未匹配 / 空白译文条目忽略；`pair.source`
/// 携带①侧源文本时写入前校验，不一致跳过并记入报告（宁漏勿错）；失败返回
/// `corrupt`。
pub fn writeback_docx(
    in_path: impl AsRef<Path>,
    pairs: &[crate::emit::TranslationPair],
    strategy: BiStrategy,
    out: impl AsRef<Path>,
) -> crate::error::Result<crate::writeback::WritebackReport> {
    let in_path = in_path.as_ref();
    let out = out.as_ref();

    // 第一遍：docforge 解析（与提取侧 register_defaults 完全相同的默认装配：
    // structure_mode=auto / include_regions=false / ocr_images=false）拿块序与 id。
    // 单元流取全量：段落单元 + 表格格单元按文档序交错（格单元由 XML 扫描的
    // w:tbl 格位配对，见模块头对齐口径）。
    let raw = DocxRawBackend::default().parse(in_path)?;
    let doc = OfficePlugin::default().assemble(&raw);
    let units = crate::emit::to_translation_units(&doc);
    let map = pair_map(pairs);

    // 第二遍：XML 扫描可翻译位（段落位 + 格位），与单元按序对齐。
    let mut zip = open_zip(in_path)?;
    let xml = crate::backends::office::zip_read_str(&mut zip, "word/document.xml")?
        .ok_or_else(|| DocError::corrupt("missing word/document.xml"))?;
    let (slots, body_close) = scan_slots(&xml)?;

    let mut report = WritebackReport::default();
    let new_xml = match strategy {
        BiStrategy::BilingualInline => apply_inline(&xml, &slots, &units, &map, &mut report),
        BiStrategy::TranslationOnly => {
            apply_translation_only(&xml, &slots, &units, &map, &mut report)
        }
        BiStrategy::OriginalPlusAppendix => {
            apply_appendix(&xml, &slots, &units, &map, body_close, &mut report)
        }
    };

    rewrite_zip(
        in_path,
        out,
        &[("word/document.xml".to_string(), new_xml.into_bytes())],
    )?;
    Ok(report)
}

/// docx 节交替：按分节符（无节则按分页符）把正文切段，每有命中的原文段后
/// 插入「该段的纯译文段」（新起一页）——PageAlt 在 Word 的最佳近似（rumi
/// 接入方需求 #12；形态对照 epub 侧 `writeback_epub_alternating`，见模块头
/// 「节交替」）。`pairs` 的 id 与 [`writeback_docx`] 同口径（同一单元流，含
/// 表格格单元，无新锚点体系；源校验同款）；全书无命中时输出 = 原包透传；
/// 失败返回 `corrupt`。
///
/// # 切段规则（body 内容字节区间；优先级从高到低）
///
/// 1. 分节边界：pPr 内含 `w:sectPr` 的段落是该节最后一块（切段含它）；
///    body 末尾裸 `w:sectPr` 之前是最后一节（译文插在裸节符之前，原节符仍
///    居末）；
/// 2. 全文仅一节时按分页符细分：段内含 `<w:br w:type="page"/>` 的段落开启
///    新段（整段归入新段——段中分页的粗粒度近似）；
/// 3. 两者皆无 → 单段整体（原文整体 + 分页 + 译文整体，合法退化）。
///
/// 译文段的节保真与分页载体见模块头（分节段靠拷贝节符自带，尾节/退化段前
/// 插独立分页段，均经金测验证重开解析合法）。
pub fn writeback_docx_alternating(
    in_path: impl AsRef<Path>,
    pairs: &[crate::emit::TranslationPair],
    out: impl AsRef<Path>,
) -> crate::error::Result<crate::writeback::WritebackReport> {
    let in_path = in_path.as_ref();
    let out = out.as_ref();

    // A. 与 writeback_docx 同一套解析 + 扫描 + 编辑（锚点体系不变）；策略
    //    固定 TranslationOnly——译文段 = 该段全部命中单元的纯译文渲染。
    let raw = DocxRawBackend::default().parse(in_path)?;
    let doc = OfficePlugin::default().assemble(&raw);
    let units = crate::emit::to_translation_units(&doc);
    let map = pair_map(pairs);

    let mut zip = open_zip(in_path)?;
    let xml = crate::backends::office::zip_read_str(&mut zip, "word/document.xml")?
        .ok_or_else(|| DocError::corrupt("missing word/document.xml"))?;
    let (slots, _) = scan_slots(&xml)?;
    let mut report = WritebackReport::default();
    let edits = translation_only_edits(&xml, &slots, &units, &map, &mut report);
    if edits.is_empty() {
        // 全书无命中：原包透传（document.xml 也不动，与 epub 交替同口径）
        return rewrite_zip(in_path, out, &[]).map(|_| report);
    }

    // B. 分段（回写侧 XML 层的 body 顶层扫描）+ 逐段拼装：原文段字节不动，
    //    有命中编辑的段后插译文段。
    let (content_start, blocks, tail_sectpr) = scan_body_blocks(&xml)?;
    let segs = segments_from_blocks(content_start, &blocks, tail_sectpr);
    let new_xml = assemble_alternating(&xml, &segs, &edits);
    rewrite_zip(
        in_path,
        out,
        &[("word/document.xml".to_string(), new_xml.into_bytes())],
    )?;
    Ok(report)
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
/// 返回子树终点字节偏移（配对闭标签之后；节交替回写的块 span 用）。
fn consume_subtree<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<usize> {
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref _e)) => depth += 1,
            Ok(Event::End(ref _e)) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(bpos(reader));
                }
            }
            Ok(Event::Eof) => return Ok(bpos(reader)),
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

/// 位 × 翻译单元按序配对（文本一致性守卫 + ①源文本校验）+ 译文查找。
/// 返回 (slot 下标, 译文) 列表，文档序；段落位与格位都在其中——
/// OriginalPlusAppendix 由调用方过滤只留段落位（表格不进附录）。
/// 守卫/源校验失败与结构漂移（单元多于位）记入 `report`——只收**有译文**
/// 者（无译文是正常未翻译，不是跳过，见 [`WritebackReport`]）。
fn matched_translations(
    slots: &[Slot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, (&str, Option<&str>)>,
    report: &mut WritebackReport,
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let n = slots.len().min(units.len());
    for k in 0..n {
        let unit = &units[k];
        // 译文查找先行：无译文/空白译文 = 正常未翻译，守卫不记报告
        let Some((tr, src_opt)) = map.get(unit.id.as_str()) else {
            continue;
        };
        let tr = tr.trim();
        if tr.is_empty() {
            continue;
        }
        // 双保险：位文本与单元文本必须一致（同一解析器同一文件的确定性保证）
        if slots[k].text().trim() != unit.text.trim() {
            report.skip(unit.id.clone(), "文本守卫不一致");
            continue;
        }
        // ①→④ 边界：译文对携带的①源文本与当前(④)文本不一致 → 跳过并记报告
        //（位置锚点在新文件里指向别的文本，写下去就是静默错位写）
        if let Some(src) = src_opt {
            if src.trim() != unit.text.trim() {
                report.skip(unit.id.clone(), "源文本不一致（提取后文件已变更）");
                continue;
            }
        }
        out.push((k, tr.to_string()));
    }
    // 结构漂移兜底（sdt 深度 quirk 等上游特性）：单元多于位 → 尾部单元无位
    // 可写，有译文的记跳过（能对到单元 id 的记 id）
    for unit in units.iter().skip(n) {
        if let Some((tr, _)) = map.get(unit.id.as_str()) {
            if !tr.trim().is_empty() {
                report.skip(unit.id.clone(), "结构限制（无可对应正文位）");
            }
        }
    }
    out
}

/// BilingualInline：在原 `</w:p>` 后插入译文段（复制原段 pPr，文本 = 译文）；
/// 格位则追在格内最后一个 `w:p` 之后（复制格内首段的 pPr；译段 run 用全新
/// 最小 rPr——仅 `w:sz`/`w:szCs` = 18 半点 = 9pt 的固定小字号，不复制原
/// run 的 rPr，见模块头；含嵌套表的格 v1 不写，见模块头）。
///
/// 段落位 pPr 的唯一改写：剔除节符（[`strip_sectpr`]）。原段是节尾段（pPr
/// 内含 `w:sectPr`，即其节的最后一块）时，verbatim 克隆会把节符复制进译文
/// 段；节符即换页边界（默认 nextPage），双语段之间会多出一页空白（节交替
/// 回写金测否决过的同款缺陷，见模块头「节保真」）。格位分支不剥：节符不落
/// 表格内（`BodyBlock.has_sectpr` 同款口径）。
fn apply_inline(
    xml: &str,
    slots: &[Slot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, (&str, Option<&str>)>,
    report: &mut WritebackReport,
) -> String {
    let matched = matched_translations(slots, units, map, report);
    if matched.is_empty() {
        return xml.to_string();
    }
    let mut edits: Vec<(usize, String)> = Vec::new();
    for (k, tr) in matched {
        match &slots[k] {
            Slot::Para(p) => {
                // pPr verbatim 克隆，唯一例外：剔除节符——原段是节尾段时复
                // 制节符 = 译文段自带换页边界，双语段间多一页空白（见
                // strip_sectpr 与函数文档）。格位分支不剥：节符不落表格内。
                let ppr = strip_sectpr(p.ppr.as_deref().unwrap_or_default());
                edits.push((
                    p.end,
                    format!(
                        "<w:p>{ppr}<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                        escape_xml_text(&tr)
                    ),
                ));
                report.written += 1;
            }
            Slot::Cell(c) => {
                // 插在最后一个 w:p 之后 = 格底追加（单段落格与「原位之后」
                // 等价）；pPr 取首段的（格内主段样式）。译文独立成段而非
                // w:br 软换行——软换行会破坏 docx 表格行高语义。
                if c.has_nested_table {
                    report.skip(units[k].id.clone(), "结构限制（嵌套表格）");
                    continue;
                }
                let Some(last) = c.paras.last() else {
                    report.skip(units[k].id.clone(), "结构限制（格内无段落）");
                    continue;
                };
                let ppr = c.paras[0].ppr.clone().unwrap_or_default();
                // 译段 run 带全新最小 rPr：固定小字号 sz/szCs = 18 半点 =
                // 9pt（窄列长译文把表格撑乱的缓解项；不复制原 run 的 rPr——
                // run 级属性不跨语言复制，字体由主题默认继承）。元素顺序按
                // OOXML CT_RPr 序列（sz 在 szCs 前），rPr 居 w:r 首位，两个
                // 子元素即天然合法；固定值不解析 styles.xml 继承链，未来可
                // 做样式感知（见模块头）。段落级 Inline 不带此 rPr（正文
                // 整段宽度充足，无需缩字号）。
                edits.push((
                    last.end,
                    format!(
                        "<w:p>{ppr}<w:r><w:rPr><w:sz w:val=\"18\"/><w:szCs w:val=\"18\"/></w:rPr>\
                         <w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                        escape_xml_text(&tr)
                    ),
                ));
                report.written += 1;
            }
        }
    }
    splice(xml, edits)
}

/// pPr 内节符的字面量形态（[`strip_sectpr`] 剥离用；与 [`W_SECTPR`] 本地
/// 名常量平行——这里要开/闭标签字节形态，单独成常量）。
const SECTPR_OPEN_TAG: &str = "<w:sectPr";
const SECTPR_CLOSE_TAG: &str = "</w:sectPr>";

/// 剔除 pPr 字符串中的节符（[`apply_inline`] 段落位克隆 pPr 的唯一改写）：
/// `<w:sectPr ...>` 到配对 `</w:sectPr>` 的闭区间、自闭合 `<w:sectPr .../>`
/// 都整段剔除，其余字节原样。动机：原段是节尾段（pPr 内含 sectPr = 其节
/// 最后一块）时，克隆节符会让追加的译文段自带换页边界（默认 nextPage）→
/// 双语段之间多出一页空白——节交替回写（`assemble_alternating`）金测分析
/// 否决过的同款缺陷，见模块头「节保真」。
///
/// sectPr 不嵌套 sectPr，开标签后首个闭标签即配对者；`<w:sectPr` 子串定位
/// 不会误命中属性值/文本——良构 XML 里 `<` 必转义为 `&lt;`。病态输入（无
/// `>` 或无配对闭标签；unreachable：`scan_slots` 已 quick-xml 全文解析成功，
/// ppr 必为良构片段）保守保留原字节。
fn strip_sectpr(ppr: &str) -> String {
    let mut out = String::with_capacity(ppr.len());
    let mut rest = ppr;
    while let Some(open) = rest.find(SECTPR_OPEN_TAG) {
        // 开标签终点 = 首个 `>`；自闭合 `/>` 到此为止，闭区间吃配对闭标签
        let Some(gt_rel) = rest[open..].find('>') else { break };
        let gt = open + gt_rel;
        let (skip_to, stripped) = if rest[..gt + 1].ends_with("/>") {
            (gt + 1, true)
        } else {
            match rest[gt + 1..].find(SECTPR_CLOSE_TAG) {
                Some(rel) => (gt + 1 + rel + SECTPR_CLOSE_TAG.len(), true),
                None => (0, false), // 无配对闭标签：保守保留原字节
            }
        };
        if !stripped {
            break;
        }
        out.push_str(&rest[..open]);
        rest = &rest[skip_to..];
    }
    out.push_str(rest);
    out
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
    map: &HashMap<&str, (&str, Option<&str>)>,
    report: &mut WritebackReport,
) -> String {
    splice_spans(xml, translation_only_edits(xml, slots, units, map, report))
}

/// TranslationOnly 的逐位置编辑（文档坐标字节区间）：段落位首 `w:t` 换译
/// 文、其余清空；格位口径见 [`apply_translation_only`] 文档（含嵌套表的格
/// / 无文本节点的位保守跳过——跳过即无编辑，节交替的「命中」判定以此为
/// 准）。[`writeback_docx`] 与 [`writeback_docx_alternating`] 共用：后者按
/// 分段区间平移取用同一编辑流，不另写渲染。报告：实际产出编辑的单元计
/// written，结构跳过记因。
///
/// 首 `w:t` 写入文本 = 配对期 trim 后的译文；当译文**原文**（map 原始值，
/// trim 之前）首/尾带空白、且其开始标签未带 `xml:space` 属性时，额外产出
/// 一个「改写开始标签补 ` xml:space="preserve"`」的编辑（判据与防御见
/// [`preserve_space_tag_edit`]）——与合成段落的 preserve 口径对齐，防
/// Word 吞空格；标签编辑与文本替换编辑区间相接不重叠，同批一次
/// [`splice_spans`] 应用（节交替平移后同样成立：两个编辑同落一个 `w:t`，
/// 永远被同一分段整体收录或整体丢弃）。
fn translation_only_edits(
    xml: &str,
    slots: &[Slot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, (&str, Option<&str>)>,
    report: &mut WritebackReport,
) -> Vec<(usize, usize, String)> {
    let matched = matched_translations(slots, units, map, report);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for (k, tr) in matched {
        // 译文原文（map 原始值，配对期 trim 之前）——xml:space 条件补的判据
        let raw_tr = map.get(units[k].id.as_str()).map(|(raw, _)| *raw);
        match &slots[k] {
            Slot::Para(p) => {
                let spans = &p.text_spans;
                if spans.is_empty() {
                    // 无文本节点可换（文本全来自 tab/br 占位）：保守跳过
                    report.skip(units[k].id.clone(), "结构限制（无文本节点）");
                    continue;
                }
                let mut it = spans.iter();
                let first = it.next().unwrap();
                edits.push((first.0, first.1, escape_xml_text(&tr)));
                edits.extend(raw_tr.and_then(|raw| preserve_space_tag_edit(xml, first.0, raw)));
                for span in it {
                    edits.push((span.0, span.1, String::new()));
                }
                report.written += 1;
            }
            Slot::Cell(c) => {
                if c.has_nested_table {
                    report.skip(units[k].id.clone(), "结构限制（嵌套表格）");
                    continue;
                }
                let mut it = c.paras.iter().flat_map(|p| p.text_spans.iter());
                let Some(first) = it.next() else {
                    // 全格无文本节点（文本全来自 tab/br 占位）：保守跳过
                    report.skip(units[k].id.clone(), "结构限制（无文本节点）");
                    continue;
                };
                edits.push((first.0, first.1, escape_xml_text(&tr)));
                edits.extend(raw_tr.and_then(|raw| preserve_space_tag_edit(xml, first.0, raw)));
                for span in it {
                    edits.push((span.0, span.1, String::new()));
                }
                report.written += 1;
            }
        }
    }
    edits
}

/// `xml:space` 条件补（TranslationOnly 首 `w:t` 原地替换，保真小项）：译文
/// 原文首/尾带空白、且该 `w:t` 开始标签未带 `xml:space` 属性时，返回「改写
/// 开始标签补 ` xml:space="preserve"`」的编辑——与合成段落的 preserve 口径
/// 对齐（合成段恒带 preserve；原地路径原来没有，Word 会吞译文边缘空格，
/// 造成排版粘连）。写入文本仍是配对期 trim 后的译文，preserve 同时护住译
/// 文内部的连续空格；两条件不同时满足则不动标签（最小侵入，输出字节零变
/// 化）。返回编辑区间 = 开始标签 `[起符, 文本起点)`，与文本替换编辑
/// `[文本起点, ..)` 相接不重叠。
///
/// 锚定与防御：文本 span 左界即开标签 `>` 之后，向前窗口（256 字节）内最近
/// `<` 必是它的开标签起符——仍校验元素**本地名**为 `t`（默认命名空间 `<t>`、
/// 换前缀 `<word:t>` 放行；防 schema 外 junk 元素错锚，如 `<w:t><w:junk>x`
/// 的 junk 标签，此时保守不加属性）；自闭合 `/>`（有文本 span 时按 schema
/// 不存在，纯防御）与已带 `xml:space`（任意引号/取值形态，子串判定）一律
/// 不动。
fn preserve_space_tag_edit(
    xml: &str,
    text_start: usize,
    raw_tr: &str,
) -> Option<(usize, usize, String)> {
    // (a) 译文原文首/尾有空白（trim 后不等）
    if raw_tr.trim() == raw_tr {
        return None;
    }
    // 开始标签区间 + 名称校验（防错锚）
    let (tag_open, _) = w_t_start_tag_span(xml, text_start)?;
    let tag = &xml[tag_open..text_start];
    let inner = &tag[1..]; // 去掉 `<`
    let qname_end = inner
        .find(|c: char| c == '>' || c == '/' || c.is_whitespace())
        .unwrap_or(inner.len());
    if inner[..qname_end].rsplit(':').next() != Some("t") {
        return None;
    }
    // (b) 已带属性（任意引号形态，子串命中）/ 自闭合：不动标签
    if tag.contains("xml:space") || tag.ends_with("/>") {
        return None;
    }
    // 属性插在收 `>` 之前（顺带收掉既有尾部空白，属性间留一个空格）
    let mut rewritten = String::with_capacity(tag.len() + 22);
    rewritten.push_str(tag[..tag.len() - 1].trim_end());
    rewritten.push_str(" xml:space=\"preserve\">");
    Some((tag_open, text_start, rewritten))
}

/// `w:t` 文本 span 的开始标签区间：span 左界（开标签 `>` 之后）向前找最近
/// 的 `<`（开标签起符——标签内部不含 `<`；`text_start` 本身是 quick-xml
/// 事件边界，必为字符边界）。窗口 256 字节（`w:t` 开标签只有 xml:space 量
/// 级的属性；窗口起点回退到字符边界再切片）；窗口内无 `<`（病态输入）返回
/// None——保守不加标签编辑。
fn w_t_start_tag_span(xml: &str, text_start: usize) -> Option<(usize, usize)> {
    const TAG_WINDOW: usize = 256;
    let mut lo = text_start.saturating_sub(TAG_WINDOW);
    while lo > 0 && !xml.is_char_boundary(lo) {
        lo -= 1;
    }
    let rel = xml[lo..text_start].rfind('<')?;
    Some((lo + rel, text_start))
}

/// OriginalPlusAppendix：正文不动，body 末尾追加「译文」标题段 + 译文段序列。
/// 表格不进附录：格位照常参与配对（消费单元流保对齐），但不产出附录条目，
/// 格文本原样不动。
fn apply_appendix(
    xml: &str,
    slots: &[Slot],
    units: &[crate::emit::TranslationUnit],
    map: &HashMap<&str, (&str, Option<&str>)>,
    body_close: usize,
    report: &mut WritebackReport,
) -> String {
    let matched = matched_translations(slots, units, map, report);
    let mut entries: Vec<String> = Vec::new();
    for (k, tr) in matched {
        if matches!(slots[k], Slot::Para(_)) {
            entries.push(tr);
        } else {
            // 格译文不进附录（附录只收段落位，正文格原样）：有译文但未写入
            report.skip(units[k].id.clone(), "结构限制（附录不含表格）");
        }
    }
    if entries.is_empty() {
        return xml.to_string();
    }
    report.written += entries.len();
    let mut appendix = String::from(
        "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr>\
         <w:r><w:t>译文</w:t></w:r></w:p>",
    );
    for tr in &entries {
        appendix.push_str(&format!(
            "<w:p><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
            escape_xml_text(tr)
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
// 节交替回写（分段扫描 + 逐段拼装；PageAlt 在 docx 的最佳近似）
// ---------------------------------------------------------------------------

/// docx 本地名补充（`backends::office::tags` 无 sectPr；本文件分段扫描唯一
/// 使用处）。
const W_SECTPR: &str = "sectPr";

/// 译文段前的独立分页段（`w:br w:type="page"` 硬分页）：尾节/退化段的「译
/// 文新起一页」语义载体（见模块头「节保真」两案取舍）。
const PAGE_BREAK_P: &str = "<w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>";

/// 一个 body 直属块（顶层元素）的字节 span 与节/分页标记（分段扫描用）。
struct BodyBlock {
    /// 块起点（开标签 `<`）。
    start: usize,
    /// 块终点（配对闭标签之后）。
    end: usize,
    /// 段落 pPr 内含 `w:sectPr`（该段是其节的最后一块；节符只落在段落 pPr
    /// 里，嵌套表内不存在 sectPr——表格块恒 false）。
    has_sectpr: bool,
    /// 段内含分页 run（`<w:br` + `w:type="page"` 双条件：纯 `<w:br/>` 软换
    /// 行不误报；Word/常见写入器的 br 属性序固定，字节探测不做属性解析）。
    page_break: bool,
}

/// 一个正文分段（[start, end) 字节区间 + 分页语义载体标记）。
struct Segment {
    /// 分段起点（body 内容起点或上一分段的终点——段间隙字节随下一段走）。
    start: usize,
    /// 分段终点：分节段 = 节符段落之后；尾段 = 裸 sectPr 起点（无则最后
    /// 一块之后 / body 内容起点）。
    end: usize,
    /// 段以带 sectPr 的段落收口（分节切的段）：译文段是原段字节拷贝，段内
    /// 自带同一节符——译文节属性逐字节保真（同页向/页眉页脚），前后换页均
    /// 由拷贝节符自带（默认 nextPage），无需任何载体段（见模块头）。
    closed_by_sectpr: bool,
    /// 段以分页段落开启（分页符细分的非首段）：译文段拷贝自带段首分页，
    /// 无需再插分页段（否则译文段首双重分页留空白页）。
    opens_with_page_break: bool,
}

/// 扫描 body 直属子级：返回 (body 内容起点, 顶层块序列, 尾部裸 sectPr 起点)。
/// 与 [`scan_slots`] 平行的轻扫描——只记块字节 span 与节/分页标记（分段是
/// 回写侧 XML 层的事，不依赖解析侧；解析侧不识别 sectPr/分页符）。顶层
/// `w:sectPr`（裸节符，OOXML 规定为 body 最后一个子级）记尾、整棵消费、不
/// 记块——其后字节全部走尾部透传。
fn scan_body_blocks(xml: &str) -> Result<(usize, Vec<BodyBlock>, Option<usize>)> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    // 定位 <w:body>（scan_slots 同款前置循环）
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

    let content_start = bpos(&reader);
    let mut blocks = Vec::new();
    let mut tail = None;
    loop {
        let pos_before = bpos(&reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().local_name().as_ref() == W_SECTPR {
                    tail = Some(pos_before);
                    consume_subtree(&mut reader)?;
                    buf.clear();
                    continue;
                }
                let end = consume_subtree(&mut reader)?;
                let span = &xml[pos_before..end];
                let is_para = e.name().local_name().as_ref() == tags::W_P;
                blocks.push(BodyBlock {
                    start: pos_before,
                    end,
                    has_sectpr: is_para && span.contains("<w:sectPr"),
                    page_break: is_para
                        && span.contains("<w:br")
                        && span.contains("w:type=\"page\""),
                });
            }
            Ok(Event::Empty(ref e)) if e.name().local_name().as_ref() == W_SECTPR => {
                // 自闭合裸节符（无节属性内容的最简形态）
                tail = Some(pos_before);
            }
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == tags::W_BODY => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML body scan error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok((content_start, blocks, tail))
}

/// 由顶层块序列装配分段（见 [`writeback_docx_alternating`] 的切段规则）：
/// 优先分节边界（sectPr 段落收段、切段含它），全文仅一节时按分页符细分
/// （含分页符的段落开启新段），两者皆无则整体一段（合法退化）。尾部再补
/// 一个尾段（裸 sectPr 之前的剩余字节；全空文档为零宽段——无编辑无插入，
/// 天然无操作）。裸 sectPr 之后的字节（违规格式）不进任何分段，走尾部透传。
fn segments_from_blocks(
    content_start: usize,
    blocks: &[BodyBlock],
    tail: Option<usize>,
) -> Vec<Segment> {
    let tail_end = tail.unwrap_or_else(|| {
        blocks
            .last()
            .map(|b| b.end)
            .unwrap_or(content_start)
    });
    let mut segs = Vec::new();

    // 1) 分节切段：连续块到带 sectPr 的段落（含它）为一段
    let mut start = content_start;
    let mut cut = false;
    for b in blocks {
        if b.has_sectpr {
            segs.push(Segment {
                start,
                end: b.end,
                closed_by_sectpr: true,
                opens_with_page_break: false,
            });
            start = b.end;
            cut = true;
        }
    }
    if !cut {
        // 2) 全文一节 → 分页符细分：含分页符的段落开启新段（整段归入新段）
        start = content_start;
        for b in blocks {
            if b.page_break {
                segs.push(Segment {
                    start,
                    end: b.start,
                    closed_by_sectpr: false,
                    opens_with_page_break: false,
                });
                start = b.start;
            }
        }
        // 尾段：起点 = 最后一个分页段（若有）→ 段首自带分页；无分页段则 =
        // 退化整段（段首无分页，译文段前插分页段）
        let tail_opens = start != content_start;
        segs.push(Segment {
            start,
            end: tail_end,
            closed_by_sectpr: false,
            opens_with_page_break: tail_opens,
        });
    } else {
        // 3) 分节形态的尾段：段首无分页（前段节符负责它自己的换页），译文
        //    段前插分页段；终点 = 裸 sectPr 起点（原节符仍居末）
        segs.push(Segment {
            start,
            end: tail_end,
            closed_by_sectpr: false,
            opens_with_page_break: false,
        });
    }
    segs
}

/// 逐段拼装：原文段字节原样复制（只插入不修改），有命中编辑的段后插译文
/// 段 = 该段 XML 子串平移套用同一套 TranslationOnly 编辑（未命中单元原样
/// 保留，译文段是完整合法的正文拷贝）；无命中段不插（避免原样页翻两遍）。
/// 编辑只落在单个块内、结构性不跨段；越界编辑保守丢弃。分页语义载体见模
/// 块头与 [`Segment`] 字段文档。
fn assemble_alternating(xml: &str, segs: &[Segment], edits: &[(usize, usize, String)]) -> String {
    let mut out = String::with_capacity(xml.len() * 2);
    let mut cursor = 0usize;
    for seg in segs {
        out.push_str(&xml[cursor..seg.start]); // 段间隙（body 开标签后的零散节点等）
        out.push_str(&xml[seg.start..seg.end]); // 原文段：字节不动
        cursor = seg.end;
        // 本段的编辑平移到段内坐标
        let own: Vec<(usize, usize, String)> = edits
            .iter()
            .filter(|(s, e, _)| *s >= seg.start && *e <= seg.end)
            .map(|(s, e, t)| (s - seg.start, e - seg.start, t.clone()))
            .collect();
        if own.is_empty() {
            continue; // 无命中段不插译文段
        }
        let tr = splice_spans(&xml[seg.start..seg.end], own);
        if seg.closed_by_sectpr || seg.opens_with_page_break {
            // 译文段是原段字节拷贝：段内自带的节符段落 / 段首分页段落提供
            // 换页与节属性保真，无需载体段（见模块头「节保真」两案取舍）
            out.push_str(&tr);
        } else {
            // 尾节/退化段：无自带分页可拷，译文段前插独立分页段新起一页；
            // 译文随下一节属性——body 裸 sectPr 的属性即本节原属性，保真等
            // 价（译文之后追加节符拷贝段会得到「原译同节连排 + 末尾空节」
            // 两个缺陷，见模块头）
            out.push_str(PAGE_BREAK_P);
            out.push_str(&tr);
        }
    }
    out.push_str(&xml[cursor..]); // 尾部透传（裸 sectPr + body/document 收尾）
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
    use crate::emit::TranslationPair;
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

    /// sdt（内容控件）fixture：表格 + sdt 内段落（sdt 位测试与枚举等价
    /// 属性测试共用）。
    const SDT_XML: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>表甲</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        r#"<w:sdt><w:sdtPr/><w:sdtContent><w:p><w:r><w:t>控件里的段落。</w:t></w:r></w:p></w:sdtContent></w:sdt>"#,
        r#"</w:body></w:document>"#
    );
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

    /// 节尾段文档：唯一段落 pPr 内含 sectPr（该段是其节的最后一块）、body
    /// 无裸节符（BilingualInline 译文段 pPr 剥离节符的考点）。
    const SECTPR_PARA_XML: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:p><w:pPr><w:sectPr><w:pgSz w:w="16838" w:h="11906" w:orient="landscape"/></w:sectPr></w:pPr><w:r><w:t>节尾段落。</w:t></w:r></w:p>"#,
        r#"</w:body></w:document>"#
    );


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
            .parse(path)
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

    fn write(
        in_path: &Path,
        out_path: &Path,
        tr: &[(&str, &str)],
        strategy: BiStrategy,
    ) -> WritebackReport {
        let pairs: Vec<TranslationPair> = tr
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()).into())
            .collect();
        writeback_docx(in_path, &pairs, strategy, out_path).unwrap()
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

    /// TranslationOnly 的 xml:space 条件补（保真小项①）：译文原文（map 原始
    /// 值）首/尾带空白 → 首 `w:t` 开始标签补 `xml:space="preserve"`（写入文
    /// 本仍为配对期 trim 后的译文，与合成段落口径一致）；译文无边缘空白 →
    /// 标签字节原样（最小侵入：全文档无任何新增 preserve）；段落位与格位
    /// 首 `w:t` 同款；重解析文本正确、结构零扰动。
    #[test]
    fn translation_only_preserve_added_only_for_edge_whitespace() {
        let dir = case_dir("tonly-space");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, DOCUMENT_XML);
        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "  Chapter 1  "),    // 边缘空白 → 补 preserve
                ("p0-b1", "Plain body"),       // 无边缘空白 → 标签不动
                ("p0-b3-r0c0", " Cell Head "), // 格位首 w:t 同款
            ],
            BiStrategy::TranslationOnly,
        );

        let out_xml = entry_str(&out_path, "word/document.xml");
        // 带边缘空白的译文：开始标签补 preserve（写入文本 = trim 后译文）
        assert!(out_xml.contains(r#"<w:t xml:space="preserve">Chapter 1</w:t>"#));
        assert!(out_xml.contains(r#"<w:t xml:space="preserve">Cell Head</w:t>"#));
        // 无边缘空白的译文：标签字节原样；全文档 preserve 恰两处（最小侵入）
        assert!(out_xml.contains("<w:t>Plain body</w:t>"));
        assert_eq!(out_xml.matches("xml:space").count(), 2);

        // 重解析：写入文本正确（trim 后译文），块数与原文一致（无插入）
        let got = blocks(&out_path);
        assert_eq!(got.len(), 5);
        assert_eq!(got[0].content, "Chapter 1");
        assert_eq!(got[1].content, "Plain body");
        let table = got[3].table.as_ref().unwrap();
        assert_eq!(
            table.rows,
            vec![vec!["Cell Head".to_string(), "表头乙".to_string()]]
        );

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

    /// BilingualInline 的节符剥离（保真小项②）：原段是节尾段（pPr 内含
    /// sectPr）时，追加的译文段复制 pPr 但**不含**节符——节符即换页边界，
    /// 复制会让双语段之间多出一页空白（节交替金测否决过的同款缺陷，见模块
    /// 头）。原段字节原样（节符保留在原段）、全文档 sectPr 恰一处、重解析
    /// 正常。
    #[test]
    fn inline_append_strips_sectpr_from_copied_ppr() {
        let dir = case_dir("inline-sectpr");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, SECTPR_PARA_XML);
        assert_eq!(unit_ids(&in_path), ["p0-b0"]);

        write(
            &in_path,
            &out_path,
            &[("p0-b0", "Sect end para.")],
            BiStrategy::BilingualInline,
        );

        let out_xml = entry_str(&out_path, "word/document.xml");
        // 原段字节原样：节符保留在原段 pPr 内
        assert!(out_xml.contains(
            r#"<w:p><w:pPr><w:sectPr><w:pgSz w:w="16838" w:h="11906" w:orient="landscape"/></w:sectPr></w:pPr><w:r><w:t>节尾段落。</w:t></w:r></w:p>"#
        ));
        // 译文段：pPr 克隆但节符已剔除（剩空 pPr）
        assert!(out_xml.contains(
            r#"<w:p><w:pPr></w:pPr><w:r><w:t xml:space="preserve">Sect end para.</w:t></w:r></w:p>"#
        ));
        // 全文档 sectPr 恰一处（= 原段节符；译文段无拷贝）
        assert_eq!(out_xml.matches("<w:sectPr").count(), 1, "译文段不得复制节符");

        // 重解析：原段 + 译段，文本正确（节符段/空 pPr 段不产额外块）
        let got = blocks(&out_path);
        assert_eq!(
            got.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            ["节尾段落。", "Sect end para."]
        );
        assert!(got.iter().all(|b| b.kind == Paragraph));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// sdt（内容控件）里的段落也是候选位；表格产格单元（未给译文 → 原样）。
    #[test]
    fn sdt_paragraphs_are_slots_and_tables_skipped() {
        let dir = case_dir("sdt");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
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
        let translated: Vec<TranslationPair> = units
            .iter()
            .filter(|u| u.id != "p0-b1-r1c1")
            .map(|u| (u.id.clone(), format!("[{}]", u.text)).into())
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
        let translated: Vec<TranslationPair> = units
            .iter()
            .map(|u| (u.id.clone(), format!("译·{}", u.text)).into())
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
        let translated: Vec<TranslationPair> = vec![
            ("p0-b0-r0c0".to_string(), "[外格]".to_string()).into(),
            ("p0-b0-r0c1".to_string(), "[普通格]".to_string()).into(),
        ];
        let xml = entry_str(&in_path, "word/document.xml");
        let (slots, _) = scan_slots(&xml).unwrap();
        let map = pair_map(&translated);
        let mut probe = WritebackReport::default();
        let matched = matched_translations(&slots, &units, &map, &mut probe);
        assert_eq!(
            matched.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            [0, 1],
            "两个格位都命中（流对齐 + 展平镜像精确）"
        );
        assert!(probe.skipped.is_empty(), "守卫通过：无跳过");

        // TranslationOnly：嵌套格不写（原样 → 记结构跳过），普通格替换
        let report =
            writeback_docx(&in_path, &translated, BiStrategy::TranslationOnly, &out_path).unwrap();
        assert_eq!(report.written, 1, "普通格写入");
        assert_eq!(
            report.skipped,
            [(
                "p0-b0-r0c0".to_string(),
                "结构限制（嵌套表格）".to_string()
            )]
        );
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
    /// `w:p` 而非 `w:br` 软换行；译段 run 带固定 9pt 小字号 rPr）；多段落格
    /// 重解析 = 原文 join 译文（提取侧格内多段落 join 同口径）。
    #[test]
    fn bilingual_inline_appends_translation_paragraph_in_cell() {
        let dir = case_dir("tbl_inline");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
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

        let translated: Vec<TranslationPair> = vec![
            ("p0-b0-r0c0".to_string(), "Cell translation".to_string()).into(),
            ("p0-b0-r0c1".to_string(), "Beta".to_string()).into(),
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
        // 追加段复制格内首段的 pPr（Heading2），独成 w:p；译段 run 带 9pt
        // 小字号（sz/szCs = 18 半点，全新最小 rPr 不复制原 run rPr）
        let out_xml = entry_str(&out_path, "word/document.xml");
        assert!(out_xml.contains(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:rPr><w:sz w:val="18"/><w:szCs w:val="18"/></w:rPr><w:t xml:space="preserve">Cell translation</w:t></w:r></w:p>"#
        ));
        // 无 pPr 的格（乙）：译段同样带小字号 rPr（rPr 居 w:r 首位）
        assert!(out_xml.contains(
            r#"<w:p><w:r><w:rPr><w:sz w:val="18"/><w:szCs w:val="18"/></w:rPr><w:t xml:space="preserve">Beta</w:t></w:r></w:p>"#
        ));
        // 译文 run 全部带 sz/szCs（每个 sz 都配对 szCs，值恒 18 半点 = 9pt）
        assert_eq!(out_xml.matches(r#"<w:sz w:val="18"/>"#).count(), 2);
        assert_eq!(out_xml.matches(r#"<w:szCs w:val="18"/>"#).count(), 2);
        // 原段 run 无 rPr（小字号只落在追加的译段上；甲段二无 pPr，字节原样）
        assert!(out_xml.contains("<w:p><w:r><w:t>甲段二</w:t></w:r></w:p>"));
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
        let translated: Vec<TranslationPair> = units
            .iter()
            .map(|u| (u.id.clone(), format!("[{}]", u.text)).into())
            .collect();
        let report =
            writeback_docx(&in_path, &translated, BiStrategy::OriginalPlusAppendix, &out_path)
                .unwrap();
        // 报告：written 只计附录写入的段落译文；格译文按设计不进附录 → 记跳过
        assert_eq!(report.written, 2, "附录只写两个段落译文");
        assert_eq!(report.skipped.len(), 4, "四个格译文记结构跳过");
        assert!(report
            .skipped
            .iter()
            .all(|(id, r)| id.starts_with("p0-b1-r") && r == "结构限制（附录不含表格）"));

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
            running: false,
        };
        let pairs: Vec<TranslationPair> = [
            ("p0-b0-r0c0".to_string(), "格译文".to_string()),
            ("p0-b1".to_string(), "段译文".to_string()),
        ]
        .iter()
        .map(|(a, b)| (a.clone(), b.clone()).into())
        .collect();
        let map = pair_map(&pairs);

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
        let mut report = WritebackReport::default();
        assert!(
            matched_translations(&slots, &units, &map, &mut report).is_empty(),
            "文本不一致的位不写"
        );
        // 守卫失败补记因（只收有译文者：两个 id 都有译文 → 都记）
        assert_eq!(
            report.skipped,
            [
                ("p0-b0-r0c0".to_string(), "文本守卫不一致".to_string()),
                ("p0-b1".to_string(), "文本守卫不一致".to_string()),
            ]
        );

        // 反例：文本一致时照常命中（守卫只把关写不写，不破坏流配对）
        let ok_units = vec![
            unit("p0-b0-r0c0", "格文本", Table),
            unit("p0-b1", "段落文本", Paragraph),
        ];
        let mut report = WritebackReport::default();
        assert_eq!(
            matched_translations(&slots, &ok_units, &map, &mut report)
                .iter()
                .map(|(k, tr)| (*k, tr.as_str()))
                .collect::<Vec<_>>(),
            [(0, "格译文"), (1, "段译文")]
        );
        assert!(
            report.skipped.is_empty(),
            "守卫通过：无跳过（written 由写入方累计，配对函数不计）"
        );
    }

    // -------------------------------------------------------------------------
    // 节交替回写（writeback_docx_alternating）
    // -------------------------------------------------------------------------

    /// 两节文档：节1 横向（landscape，pPr 内 sectPr 的段落收节）、节2 纵向
    /// （body 裸 sectPr 收尾）。
    const MULTI_SECTION_XML: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:p><w:r><w:t>第一节第一段。</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:sectPr><w:pgSz w:w="16838" w:h="11906" w:orient="landscape"/></w:sectPr></w:pPr><w:r><w:t>第一节节尾段。</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>第二节首段。</w:t></w:r></w:p>"#,
        r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr>"#,
        r#"</w:body></w:document>"#
    );

    /// 单节无节符无分页的退化文档（原文整体 + 分页 + 译文整体）。
    const PLAIN_XML: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:p><w:r><w:t>甲段正文。</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>乙段正文。</w:t></w:r></w:p>"#,
        r#"</w:body></w:document>"#
    );

    /// 单节多分页符文档：第二段以 `<w:br w:type="page"/>` 开启新页（整段归
    /// 入新段的粗粒度近似考点）。
    const PAGE_BREAK_XML: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:p><w:r><w:t>甲页正文。</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:br w:type="page"/></w:r><w:r><w:t>乙页首段。</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>乙页次段。</w:t></w:r></w:p>"#,
        r#"</w:body></w:document>"#
    );

    /// 分节 + 表格文档：表格在节1 内（格单元译文进译文段的考点）。
    const TABLE_SECTION_XML: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
        r#"<w:p><w:r><w:t>表格前的段落。</w:t></w:r></w:p>"#,
        r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>名称</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>数值</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
        r#"<w:p><w:pPr><w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr></w:pPr><w:r><w:t>第一节节尾。</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>第二节独段。</w:t></w:r></w:p>"#,
        r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr>"#,
        r#"</w:body></w:document>"#
    );

    /// alternating 便捷入口（与 write 同款）。
    fn alternating_write(in_path: &Path, out_path: &Path, tr: &[(&str, &str)]) -> WritebackReport {
        let pairs: Vec<TranslationPair> = tr
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()).into())
            .collect();
        writeback_docx_alternating(in_path, &pairs, out_path).unwrap()
    }

    /// 金测（核心验收）：节序 = 原1/译1/原2/译2、译文段 = 该段 TranslationOnly
    /// 结果、原文段字节与输入一致、重解析（docforge parse = 重开 zip 解析）
    /// 合法且文本正确；裸 sectPr 仍居末。
    #[test]
    fn alternating_sections_roundtrip() {
        let dir = case_dir("alt-sections");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, MULTI_SECTION_XML);

        // 与 writeback_docx 同一单元流（无新锚点体系）
        assert_eq!(unit_ids(&in_path), ["p0-b0", "p0-b1", "p0-b2"]);

        alternating_write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "Section 1 para one."),
                ("p0-b1", "Section 1 last para."),
                ("p0-b2", "Section 2 first para."),
            ],
        );

        // 重解析：原/译按节交替（整段原文在前、整段译文在后；节符段、译
        // 文拷贝节符段、分页段都是空段不产块——解析侧 trim 非空才发块）
        let got = blocks(&out_path);
        assert_eq!(
            got.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            [
                "第一节第一段。",
                "第一节节尾段。",
                "Section 1 para one.",
                "Section 1 last para.",
                "第二节首段。",
                "Section 2 first para.",
            ]
        );
        assert!(got.iter().all(|b| b.kind == Paragraph));

        // 原文段字节与输入一致（只插入，不修改既有内容）
        let out_xml = entry_str(&out_path, "word/document.xml");
        assert!(out_xml.contains(
            r#"<w:p><w:pPr><w:sectPr><w:pgSz w:w="16838" w:h="11906" w:orient="landscape"/></w:sectPr></w:pPr><w:r><w:t>第一节节尾段。</w:t></w:r></w:p>"#
        ));
        assert!(out_xml.contains("<w:p><w:r><w:t>第二节首段。</w:t></w:r></w:p>"));

        // 结构不变量：landscape 节符 = 原 + 译文拷贝各一；尾节译文前的分页
        // 载体段仅一处；裸 sectPr 仍在 body 末尾且位于译文之后
        const LANDSCAPE: &str =
            r#"<w:sectPr><w:pgSz w:w="16838" w:h="11906" w:orient="landscape"/></w:sectPr>"#;
        assert_eq!(out_xml.matches(LANDSCAPE).count(), 2);
        assert_eq!(out_xml.matches(PAGE_BREAK_P).count(), 1);
        let pbr = out_xml.find(PAGE_BREAK_P).unwrap();
        let p2 = out_xml.find("第二节首段。").unwrap();
        assert!(pbr > p2, "分页段在第二节原文之后");
        let sect = out_xml.rfind("<w:sectPr").unwrap();
        let tr2 = out_xml.rfind("Section 2 first para.").unwrap();
        assert!(tr2 < sect, "尾节译文插在裸 sectPr 之前");
        assert!(out_xml[sect..].contains("</w:body>"), "裸 sectPr 仍收尾");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 节保真：横向节的译文段 sectPr 与原节逐字节相同（译文段为字节拷贝
    /// 自带节符、不追加载体段——设计取舍见模块头）；重开解析合法。
    #[test]
    fn alternating_translated_section_keeps_landscape_sectpr() {
        let dir = case_dir("alt-landscape");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, MULTI_SECTION_XML);
        // 只喂节1：节2 无命中不插译文段
        alternating_write(&in_path, &out_path, &[("p0-b0", "S1 P1."), ("p0-b1", "S1 P2.")]);

        let out_xml = entry_str(&out_path, "word/document.xml");
        // 译文段内的拷贝节符与原节符逐字节一致（同为横向：原 + 拷贝各一）
        const LANDSCAPE: &str =
            r#"<w:sectPr><w:pgSz w:w="16838" w:h="11906" w:orient="landscape"/></w:sectPr>"#;
        assert_eq!(out_xml.matches(LANDSCAPE).count(), 2);
        // 译文节尾段 = 原段字节拷贝换文本（节符拷贝随拷贝而来）
        assert!(out_xml.contains(
            r#"<w:p><w:pPr><w:sectPr><w:pgSz w:w="16838" w:h="11906" w:orient="landscape"/></w:sectPr></w:pPr><w:r><w:t>S1 P2.</w:t></w:r></w:p>"#
        ));
        // 无命中段不插：第二节译文不存在、无分页载体段
        assert!(!out_xml.contains("Section 2"));
        assert_eq!(out_xml.matches(PAGE_BREAK_P).count(), 0);
        // 重开解析合法：原 3 块 + 节1 译文 2 块（整段原文在前、整段译文在后）
        let got = blocks(&out_path);
        assert_eq!(
            got.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            ["第一节第一段。", "第一节节尾段。", "S1 P1.", "S1 P2.", "第二节首段。"]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 单节退化：无 sectPr 无分页 → 原文整体 + 独立分页段 + 译文整体。
    #[test]
    fn alternating_single_segment_degenerate() {
        let dir = case_dir("alt-degenerate");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, PLAIN_XML);
        alternating_write(
            &in_path,
            &out_path,
            &[("p0-b0", "Alpha body."), ("p0-b1", "Beta body.")],
        );

        let out_xml = entry_str(&out_path, "word/document.xml");
        // 插入结构：原文整体 + 分页段（恰一处，夹在原文与译文之间）+ 译文整体
        assert_eq!(out_xml.matches(PAGE_BREAK_P).count(), 1);
        assert!(out_xml.contains(&format!(
            "<w:p><w:r><w:t>乙段正文。</w:t></w:r></w:p>{PAGE_BREAK_P}\
             <w:p><w:r><w:t>Alpha body.</w:t></w:r></w:p>"
        )));
        // 重解析：原 2 块 + 译 2 块
        let got = blocks(&out_path);
        assert_eq!(
            got.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            ["甲段正文。", "乙段正文。", "Alpha body.", "Beta body."]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 分页符细分：单节多分页符 → 按分页符切段交替；以分页段落开启的段，
    /// 译文段拷贝自带段首分页、不重复插分页段（否则译文段首双重分页留白页）。
    #[test]
    fn alternating_splits_by_page_breaks() {
        let dir = case_dir("alt-pagebreak");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, PAGE_BREAK_XML);
        // 单元流同 writeback_docx（分页 br 是文本占位 \n，不产生额外块）
        assert_eq!(unit_ids(&in_path), ["p0-b0", "p0-b1", "p0-b2"]);

        alternating_write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "Alpha page."),
                ("p0-b1", "Beta first."),
                ("p0-b2", "Beta second."),
            ],
        );

        // 重解析：甲页原/译 + 乙页原（两段）/译（两段）
        let got = blocks(&out_path);
        assert_eq!(
            got.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            [
                "甲页正文。",
                "Alpha page.",
                "乙页首段。",
                "乙页次段。",
                "Beta first.",
                "Beta second.",
            ]
        );

        // 结构：甲页段（段首无分页）译文前插分页载体段恰一处；乙页段译文
        // 自带拷贝的段首分页。br 计 3 = 分页载体段 1 + 原乙页段 1 + 译文乙
        // 页首段 1
        let out_xml = entry_str(&out_path, "word/document.xml");
        assert_eq!(out_xml.matches(PAGE_BREAK_P).count(), 1);
        assert_eq!(out_xml.matches(r#"<w:br w:type="page"/>"#).count(), 3);
        // 译文乙页首段 = 原段字节拷贝换文本（段首分页随拷贝而来）
        assert!(out_xml.contains(
            r#"<w:p><w:r><w:br w:type="page"/></w:r><w:r><w:t>Beta first.</w:t></w:r></w:p>"#
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 部分命中：无命中段不插译文段（避免原样页翻两遍）；全书无命中 →
    /// 原包透传（document.xml 也不动）。
    #[test]
    fn alternating_partial_hit_and_no_hit_passthrough() {
        let dir = case_dir("alt-partial");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, MULTI_SECTION_XML);

        // 只喂节1 首段：节2 无命中 → 不插译文段、无分页载体段
        alternating_write(&in_path, &out_path, &[("p0-b0", "S1 P1.")]);
        let got = blocks(&out_path);
        assert_eq!(
            got.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            [
                "第一节第一段。",
                "第一节节尾段。", // 节1 整段原文在前
                "S1 P1.",
                "第一节节尾段。", // 节1 译文段内的未命中原样拷贝（译文段是完整正文拷贝）
                "第二节首段。",
            ]
        );
        let out_xml = entry_str(&out_path, "word/document.xml");
        assert!(!out_xml.contains("Section 2"), "无命中段不插译文");
        assert_eq!(out_xml.matches(PAGE_BREAK_P).count(), 0, "尾节无命中：无分页段");

        // 全书无命中：输出 = 原包透传
        let out2 = dir.join("empty.docx");
        alternating_write(&in_path, &out2, &[]);
        assert_eq!(blocks(&out2), blocks(&in_path));
        assert_eq!(
            entry_str(&in_path, "word/document.xml"),
            entry_str(&out2, "word/document.xml"),
            "无命中连 document.xml 都不动"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 含表格段：格单元译文进译文段（既有编辑流按字节区间归属段、平移套
    /// 用，表格格位口径不变），原文段内的表格原样。
    #[test]
    fn alternating_table_cell_edits_flow_into_translated_segment() {
        let dir = case_dir("alt-table");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, TABLE_SECTION_XML);

        // 同一单元流：段落 + 格位交错（无新锚点）
        assert_eq!(
            unit_ids(&in_path),
            ["p0-b0", "p0-b1-r0c0", "p0-b1-r0c1", "p0-b2", "p0-b3"]
        );

        alternating_write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "Before the table."),
                ("p0-b1-r0c0", "Name"),
                ("p0-b1-r0c1", "Value"),
                ("p0-b2", "Section 1 end."),
                // p0-b3 不喂：第二节无命中不插
            ],
        );

        // 重解析：原段（段+表+节尾）→ 译文段（段+表+节尾，格与段落全部
        // 译文）→ 第二节原段（无命中）
        let got = blocks(&out_path);
        assert_eq!(got.len(), 7);
        assert_eq!(got[0].content, "表格前的段落。");
        let orig_tbl = got[1].table.as_ref().unwrap();
        assert_eq!(
            orig_tbl.rows,
            vec![vec!["名称".to_string(), "数值".to_string()]]
        );
        assert_eq!(got[2].content, "第一节节尾。");
        assert_eq!(got[3].content, "Before the table.");
        let tr_tbl = got[4].table.as_ref().unwrap();
        assert_eq!(
            tr_tbl.rows,
            vec![vec!["Name".to_string(), "Value".to_string()]]
        );
        assert_eq!(got[5].content, "Section 1 end.");
        assert_eq!(got[6].content, "第二节独段。");

        // 结构不变量：表格两份（原 + 译文拷贝）；节符三份（节1 原 + 译文拷
        // 贝 + 裸 sectPr）；尾节无命中 → 无分页载体段
        let out_xml = entry_str(&out_path, "word/document.xml");
        assert_eq!(out_xml.matches("<w:tbl>").count(), 2);
        assert_eq!(out_xml.matches("<w:sectPr>").count(), 3);
        assert_eq!(out_xml.matches(PAGE_BREAK_P).count(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------------------
    // 译文对（TranslationPair）源校验 + 报告
    // -------------------------------------------------------------------------

    /// 源校验一致路径：携带①侧源文本（= to_translation_units 文本）的译文
    /// 对，输出与不带源（迁移前 `(id, 译文)` 形态）逐字节等价；written 计数
    /// 齐全（含格）。
    #[test]
    fn source_validated_pairs_produce_identical_output() {
        let dir = case_dir("src-ok");
        let in_path = dir.join("in.docx");
        let a = dir.join("a.docx");
        let b = dir.join("b.docx");
        build_docx(&in_path, DOCUMENT_XML);
        let units = to_translation_units(&parse_doc(&in_path));

        // 不带源：`(id, 译文)` 二元组 From（= 迁移前形态）
        let plain: Vec<TranslationPair> = units
            .iter()
            .map(|u| (u.id.clone(), format!("译·{}", u.text)).into())
            .collect();
        let rep_plain = writeback_docx(&in_path, &plain, BiStrategy::TranslationOnly, &a).unwrap();

        // 带一致源文本：`(id, 源, 译文)` 三元组 From
        let checked: Vec<TranslationPair> = units
            .iter()
            .map(|u| (u.id.clone(), u.text.clone(), format!("译·{}", u.text)).into())
            .collect();
        let rep_checked =
            writeback_docx(&in_path, &checked, BiStrategy::TranslationOnly, &b).unwrap();

        // 逐字节等价：document.xml（其余部件本就透传）
        assert_eq!(
            entry_str(&a, "word/document.xml"),
            entry_str(&b, "word/document.xml"),
            "源一致 ⇒ 与无源形态输出逐字节一致"
        );
        assert_eq!(rep_checked.written, units.len(), "全部单元写入（含格）");
        assert!(rep_checked.skipped.is_empty(), "一致源不产生跳过");
        assert_eq!(rep_plain.written, rep_checked.written);
        assert_eq!(rep_plain.skipped, rep_checked.skipped);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 源校验失败路径：①源文本 ≠ ④文本 → 该单元跳过（报告记 id 与原因）、
    /// 该处原文保留；段落与表格格各一例；一致单元照常写入（部分命中计数）。
    #[test]
    fn source_mismatch_skips_unit_keeps_original() {
        let dir = case_dir("src-bad");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, DOCUMENT_XML);
        let units = to_translation_units(&parse_doc(&in_path));

        // p0-b1（段落）与 p0-b3-r0c0（表格格）的①源文本被篡改；其余一致
        let bad = ["p0-b1", "p0-b3-r0c0"];
        let pairs: Vec<TranslationPair> = units
            .iter()
            .map(|u| {
                let src = if bad.contains(&u.id.as_str()) {
                    format!("（提取后文件已变更）{}", u.text)
                } else {
                    u.text.clone()
                };
                (u.id.clone(), src, format!("译·{}", u.text)).into()
            })
            .collect();
        let report =
            writeback_docx(&in_path, &pairs, BiStrategy::BilingualInline, &out_path).unwrap();

        assert_eq!(report.written, 3, "一致的 3 个单元写入（段 + 格 + 段）");
        assert_eq!(
            report.skipped,
            [
                (
                    "p0-b1".to_string(),
                    "源文本不一致（提取后文件已变更）".to_string()
                ),
                (
                    "p0-b3-r0c0".to_string(),
                    "源文本不一致（提取后文件已变更）".to_string()
                ),
            ],
            "跳过按文档序记 id 与原因"
        );

        // 跳过单元该处原文：段落无译邻段、格文本原样
        let got = blocks(&out_path);
        assert!(
            !got.iter().any(|b| b.content.contains("译·这是第一段")),
            "源不一致段落不写：{:?}",
            got.iter().map(|b| b.content.as_str()).collect::<Vec<_>>()
        );
        let table = got.iter().find(|b| b.table.is_some()).unwrap();
        assert_eq!(
            table.table.as_ref().unwrap().rows[0][0],
            "表头甲",
            "源不一致格原样"
        );
        // 一致单元照常写入：段落独立成块；表格格（Inline = 格内追加 w:p）
        // 重解析后格文本 = 原文 join 译文（格内段落拼接），落在 Table.rows
        assert!(got.iter().any(|b| b.content == "译·第一章 引言"));
        assert!(
            table
                .table
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .any(|r| r.iter().any(|c| c.contains("译·表头乙"))),
            "一致格写入（格文本 join 形态）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 报告计数（部分命中 + 混合跳过原因）：源校验跳过与结构跳过（嵌套
    /// 表格格）同场；written 只计实际写入；alternating 同口径透传并累计。
    #[test]
    fn report_counts_partial_hits_and_mixed_skips() {
        let dir = case_dir("report-mix");
        let in_path = dir.join("in.docx");
        let out_path = dir.join("out.docx");
        build_docx(&in_path, NESTED_XML);
        let units = to_translation_units(&parse_doc(&in_path));
        assert_eq!(units.len(), 2, "嵌套格 + 普通格");

        // 嵌套格源一致但结构不写；普通格源不一致
        let pairs: Vec<TranslationPair> = vec![
            (
                "p0-b0-r0c0".to_string(),
                "外格\n内甲,内乙\n尾段".to_string(),
                "[外格]".to_string(),
            )
                .into(),
            (
                "p0-b0-r0c1".to_string(),
                "（已变更）普通格".to_string(),
                "[普通格]".to_string(),
            )
                .into(),
        ];
        let report =
            writeback_docx(&in_path, &pairs, BiStrategy::TranslationOnly, &out_path).unwrap();

        assert_eq!(report.written, 0, "全部未写入");
        assert_eq!(
            report.skipped,
            [
                (
                    "p0-b0-r0c1".to_string(),
                    "源文本不一致（提取后文件已变更）".to_string()
                ),
                ("p0-b0-r0c0".to_string(), "结构限制（嵌套表格）".to_string()),
            ],
            "混合跳过原因（配对期源校验在前、写入期结构跳过在后）"
        );
        // 全跳过 → 无编辑 → 输出原样
        assert_eq!(blocks(&out_path), blocks(&in_path));

        // alternating 同口径：全跳过 → 无命中 → 原包透传，报告照常累计
        let alt = dir.join("alt.docx");
        let rep_alt = writeback_docx_alternating(&in_path, &pairs, &alt).unwrap();
        assert_eq!(rep_alt.written, 0);
        assert_eq!(rep_alt.skipped, report.skipped);
        assert_eq!(
            entry_str(&alt, "word/document.xml"),
            entry_str(&in_path, "word/document.xml"),
            "无命中连 document.xml 都不动"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// strip_sectpr 形态覆盖（保真小项②的纯函数面）：闭区间 / 自闭合 /
    /// 带属性自闭合 / 前后内容保留 / 无节符字节原样 / 病态（无配对闭标签）
    /// 保守保留。
    #[test]
    fn strip_sectpr_covers_all_forms() {
        assert_eq!(
            strip_sectpr(r#"<w:pPr><w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr></w:pPr>"#),
            "<w:pPr></w:pPr>"
        );
        // 自闭合与带属性自闭合（防御分支；常见形态是闭区间）
        assert_eq!(strip_sectpr("<w:pPr><w:sectPr/></w:pPr>"), "<w:pPr></w:pPr>");
        assert_eq!(
            strip_sectpr(r#"<w:pPr><w:sectPr w:rsidR="00A1B2C3"/></w:pPr>"#),
            "<w:pPr></w:pPr>"
        );
        // 节符前后的 pPr 子元素逐字节保留
        assert_eq!(
            strip_sectpr(
                r#"<w:pPr><w:pStyle w:val="H1"/><w:sectPr><w:pgSz/></w:sectPr><w:spacing/></w:pPr>"#
            ),
            r#"<w:pPr><w:pStyle w:val="H1"/><w:spacing/></w:pPr>"#
        );
        // 无节符 / 空串：字节原样
        assert_eq!(
            strip_sectpr(r#"<w:pPr><w:pStyle w:val="H1"/></w:pPr>"#),
            r#"<w:pPr><w:pStyle w:val="H1"/></w:pPr>"#
        );
        assert_eq!(strip_sectpr(""), "");
        // 病态（unreachable：scan_slots 已全文解析成功）：保守保留原字节
        assert_eq!(
            strip_sectpr("<w:pPr><w:sectPr></w:pPr>"),
            "<w:pPr><w:sectPr></w:pPr>"
        );
    }

    /// preserve_space_tag_edit 判据与区间（保真小项①的纯函数面）：原文边缘
    /// 空白 × 标签无属性 → 改写编辑（区间 = 开始标签，止于文本起点）；无边
    /// 缘空白 / 标签已带属性（任意形态）/ 错锚（span 前最近 `<` 非 t 元素）
    /// → None。
    #[test]
    fn preserve_space_tag_edit_gate_and_interval() {
        let xml = r#"<w:p><w:r><w:t>甲</w:t><w:t xml:space="preserve"> 乙 </w:t></w:r></w:p>"#;
        // 首个 w:t：文本起点 = `<w:t>` 之后；带边缘空白 → 补属性，
        // 编辑区间恰为开始标签 `[<, 文本起点)`
        let first_t = xml.find(">甲").unwrap() + 1;
        let (s, e, tag) = preserve_space_tag_edit(xml, first_t, " EN ").unwrap();
        assert_eq!(&xml[s..e], "<w:t>");
        assert_eq!(tag, "<w:t xml:space=\"preserve\">");
        assert_eq!(e, first_t, "标签编辑止于文本起点：与文本替换编辑区间相接不重叠");
        // 无边缘空白 → None（最小侵入）
        assert!(preserve_space_tag_edit(xml, first_t, "EN").is_none());
        // 标签已带 xml:space（第二个 w:t，任意形态子串命中）→ None
        let second_t = xml.find("> 乙 ").unwrap() + 1;
        assert!(preserve_space_tag_edit(xml, second_t, " EN ").is_none());
        // 错锚防御：span 左界之前最近 `<` 是 `<w:r>`（非 t 元素）→ None
        let run_end = xml.find("<w:r>").unwrap() + "<w:r>".len();
        assert!(preserve_space_tag_edit(xml, run_end, " EN ").is_none());
    }

    /// 枚举等价（解析器↔扫描器漂移报警器）：对全部 docx fixture 显式断言
    /// 「扫描器槽位文本序列 == to_translation_units 文本序列」——两侧都来自
    /// ④的同一文件；roundtrip 金测只隐式覆盖该不变量，这里独立钉死。
    #[test]
    fn scanner_slot_texts_enumerate_same_as_translation_units() {
        let fixtures: Vec<(&str, &str)> = vec![
            ("basic", DOCUMENT_XML),
            ("sectpr", SECTPR_XML),
            ("table", TABLE_DOC_XML),
            ("gridspan", SPAN_XML),
            ("nested", NESTED_XML),
            ("cell-inline", CELL_INLINE_XML),
            ("sdt", SDT_XML),
            ("multi-section", MULTI_SECTION_XML),
            ("plain", PLAIN_XML),
            ("page-break", PAGE_BREAK_XML),
            ("table-section", TABLE_SECTION_XML),
        ];
        let dir = case_dir("enum-equiv");
        for (name, xml) in fixtures {
            let path = dir.join(format!("{name}.docx"));
            build_docx(&path, xml);
            let (slots, _) = scan_slots(xml).unwrap();
            let slot_texts: Vec<&str> = slots.iter().map(|s| s.text().trim()).collect();
            let doc = parse_doc(&path);
            let units = to_translation_units(&doc);
            let unit_texts: Vec<&str> = units.iter().map(|u| u.text.trim()).collect();
            assert_eq!(
                slot_texts, unit_texts,
                "fixture {name}: 解析器↔扫描器枚举漂移（宁在此响亮失败，不错位写）"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
