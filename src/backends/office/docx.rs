// 吸收自 docparser-office 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① crate 引用重写（docparser_core→docforge、office 内部→super）；
// ② parse_with_spans 旁路增强：同一遍解析顺带发号产回写槽位表 DocxSlotSidecar
//    （SourceToken 信物 + 字节 span，发号规则见类型文档；只加字段不改任何提取行为，
//     影子割接见 writeback/docx.rs）；
// ③ mc:Fallback 子树跳过（β 红利缺陷①）：Word 对形状/文本框写 AlternateContent 时
//    Choice（现代渲染）与 Fallback（VML 兼容）内容完全相同，旧实现两分支的 w:t 都
//    进文本（同句两单元、双语回写重复段）。现在 Fallback 整棵子树跳过——文本提取与
//    信物 span 同步跳过，只提取/改写 Choice 分支。取舍：现代渲染器全覆盖；极老阅读器
//    看到未译的 VML Fallback 可接受（Fallback 字节原样保留，不毁文档）。
// ④ 段落槽位超集字段（β 红利缺陷②，回写侧双路径见 writeback/docx.rs）：wt_spans
//    （w:t 全元素区间）、dominant_rpr_span（主导 run 直属 rPr 区间，选举口径见
//    extract_para）、has_non_text_inline（非文本内联物件判据）、inner_content（段净荷
//    区间）。解析只采集，消费全在回写侧。
//! DOCX 解析：ZIP + quick-xml 流式文本提取（与 Python `_docx_raw_stream` 语义对齐）。
//!
//! 架构：
//!   `DocxRawBackend` -> `ParseCapability`：打开 OOXML ZIP，流式读取 `word/document.xml`，
//!   提取段落文本与表格行；段落样式用于判定标题层级（`w:pStyle` / `w:outlineLvl`）与
//!   列表（`w:numPr` + `word/numbering.xml` 的 `numFmt`），缩进/编号文本等更深结构不还原。

use std::collections::HashMap;
use std::path::Path;

use crate::error::{DocError, Result};
use crate::model::SourceToken;
use crate::plugin::{ParseCapability, RawDoc, RawPage, RawRegion};
use quick_xml::events::Event;
use quick_xml::Reader;

use super::attr_by;
use super::structure::{docx_has_structure, StructureMode};
use super::zip_read_str;

// 本文件局部元素本地名（mod.rs 的 tags 表没有这些；只服务 Fallback 跳过与主导
// rPr 选举，不进共享常量表）。比较口径与 tags 同款：`local_name().as_ref()`。
/// `<mc:Fallback>`（AlternateContent 的 VML 兼容分支，缺陷①跳过对象）。
const TAG_FALLBACK: &str = "Fallback";
/// `<w:rPr>`（run 直属字符属性，主导选举的采样对象）。
const TAG_RPR: &str = "rPr";
/// `<w:vertAlign>`（上下标；含它的 run 不参选主导选举）。
const TAG_VERT_ALIGN: &str = "vertAlign";
/// 段内「非文本内联物件」本地名（`has_non_text_inline` 判据的一部分）：含其一
/// 的段落翻译净荷不能整体替换（会毁掉图/框/域/注记），回写侧据此走保真路径。
/// 布局承载元素（`w:tab`/`w:br`（含软换行与分页）/`w:cr`/`w:noBreakHyphen`）
/// 不在本表、在 extract_para 的占位分支就地触发——它们在**提取**里是 \t/\n/-
/// 文本占位，但在**渲染**里是布局元素（旧写入路径保留其 run，快路径整段替换
/// 会删掉：软换行段落变单行、tab 位/分页符丢失）。
const NON_TEXT_INLINE_TAGS: [&str; 7] = [
    "drawing",
    "pict",
    "object",
    "oMath",
    "footnoteReference",
    "endnoteReference",
    "fldSimple",
];

/// DOCX raw 后端：跳过结构还原，直接提取文本。
#[derive(Default)]
pub struct DocxRawBackend {
    /// 是否解析区域部件（页眉/页脚/脚注/尾注）。默认关：每个区域部件都是一次
    /// XML 解析，默认路径不为它付费（对齐 Python `ctx.options["include_regions"]`）。
    pub include_regions: bool,
    /// 结构还原三态（对齐 Python `policy.structure_mode`）：非语义时整篇压成
    /// 行文本（`RawPage::flat_text`），组装层按行产 `Paragraph` 块（无结构块）。
    pub structure_mode: StructureMode,
    /// 是否提取内嵌位图（对齐 Python `policy.ocr_images`，默认关）：命中
    /// `word/media/*` 白名单且足够大的图按文档序产「图片项」。
    pub ocr_images: bool,
}

impl DocxRawBackend {
    pub fn new(include_regions: bool) -> Self {
        Self {
            include_regions,
            structure_mode: StructureMode::default(),
            ocr_images: false,
        }
    }

    /// 设置结构还原三态（语义同 Python `policy.structure_mode`）。
    pub fn with_structure_mode(mut self, mode: StructureMode) -> Self {
        self.structure_mode = mode;
        self
    }

    /// 设置是否提取内嵌位图（语义同 Python `policy.ocr_images`）。
    pub fn with_ocr_images(mut self, yes: bool) -> Self {
        self.ocr_images = yes;
        self
    }

    /// [`ParseCapability::parse`] 的旁路增强：同一遍解析顺带产出回写槽位表
    /// [`DocxSlotSidecar`]。RawDoc 与 `parse` 逐字节一致（发号只加信物字段、
    /// 不改任何既有行为）；槽位表供 docx 回写器按 [`SourceToken`] 区间寻址，
    /// 替代独立的二次 XML 扫描（影子割接见 `writeback/docx.rs`）。crate 内用：
    /// 槽位表是 docx 回写专属契约，不进公共 API 面。
    pub(crate) fn parse_with_spans(&self, path: &Path) -> Result<(RawDoc, DocxSlotSidecar)> {
        let file = std::fs::File::open(path)
            .map_err(|e| DocError::corrupt(format!("cannot open {}: {e}", path.display())))?;
        let mut zip = zip::ZipArchive::new(file)
            .map_err(|e| DocError::corrupt(format!("ZIP open failed: {e}")))?;

        let xml = zip_read_str(&mut zip, "word/document.xml")?
            .ok_or_else(|| DocError::corrupt("missing word/document.xml"))?;

        // cheap probe（对齐 Python：在原始字节里找结构信号）→ 决定是否结构还原
        let semantic = self.structure_mode.resolve(docx_has_structure(&xml));

        // numbering.xml 缺失/损坏时按空表处理：带 w:numPr 的段落仍算列表，但一律无序
        // （与 Python `_DocxNumbering` 宽容解析一致，不阻断整篇）。
        // 非语义模式下列表不会成块，连 numbering 都不必解析（Python 同样跳过）。
        let numbering = if semantic {
            zip_read_str(&mut zip, "word/numbering.xml")?
                .map(|x| Numbering::parse(&x))
                .unwrap_or_default()
        } else {
            Numbering::default()
        };
        // styles.xml 的 styleId→标题层级映射（docforge 补丁：zh-CN 数字 styleId）；
        // 缺失/损坏按空表，pStyle 直接判定不受影响
        let styles = zip_read_str(&mut zip, "word/styles.xml")?
            .map(|x| parse_styles_heading_map(&x))
            .unwrap_or_default();

        let (nodes, sidecar) = docx_raw_text_with_slots(&xml, &numbering, &styles)?;

        // 内嵌图（opt-in）：rels 只在需要时读；按部件名去重（同 Python `media_seen`），
        // 去重先于「是否过小」判定——同一张太小的图不会被二次尝试。
        //
        // 只在「慢路径」收集：Python 的 docx 快路径（`not semantic and _HAVE_LXML and
        // not include_regions`）直接 return，`page.images` 恒为空——raw 模式下即便开了
        // `ocr_images` 也没有图。开了 `include_regions` 才落回慢路径（`_walk`），此时
        // raw 模式同样会收集（图片项 text 为空，不进 flat_text）。
        let want_images = self.ocr_images && (semantic || self.include_regions);
        let rels = if want_images {
            zip_read_str(&mut zip, "word/_rels/document.xml.rels")?
                .map(|x| super::rel_entries(&x))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut items: Vec<crate::plugin::RawTextItem> = Vec::new();
        {
            // 整篇一个去重域（对齐 Python `media_seen` 在 `_docx_items` 的作用范围）
            let mut reader = super::image::ImageReader::new(&mut zip);
            for node in nodes {
                match node {
                    DocxNode::Item(it) => items.push(it),
                    DocxNode::ImageRids(rids) => {
                        for rid in rids {
                            let Some((_, _, target)) = rels.iter().find(|(id, _, _)| *id == rid)
                            else {
                                continue;
                            };
                            let part = super::resolve_part("word", target);
                            if let Some(im) = reader.load(&part) {
                                items.push(super::image::image_item(im));
                            }
                        }
                    }
                }
            }
        }

        // 区域部件只在 opt-in 时解析（与 Python 同一取舍：默认连解析都不做）
        let regions = if self.include_regions {
            docx_regions(&mut zip)
        } else {
            Vec::new()
        };

        // 非语义：整篇压成行文本（表格按 `行内 join(",") / 行间 \n` 展开，与 Python
        // `"\n".join(",".join(r) for r in rows)` 一致），assemble 侧按行产纯
        // `Paragraph` 块（刻意偏离 Python 零块形态的理由见 `assemble` 内注释）。
        // items 仍保留（图片项要走它才能进 `Page.images`），图片项的 text 为空，
        // 所以不会被拼进行文本；`flat_text_of` 天然跳过。
        let flat_text = if semantic {
            None
        } else {
            Some(flat_text_of(&items))
        };

        let page = RawPage {
            name: None,
            items,
            flat_text,
            regions,
            ..Default::default()
        };
        Ok((
            RawDoc {
                pages: vec![page],
                ..Default::default()
            },
            sidecar,
        ))
    }
}

impl ParseCapability for DocxRawBackend {
    fn id(&self) -> &str {
        "office-raw"
    }
    fn formats(&self) -> &[&str] {
        &["docx"]
    }
    fn parse(&self, path: &Path) -> Result<RawDoc> {
        self.parse_with_spans(path).map(|(raw, _)| raw)
    }
}

/// 段落逐条拼接成 raw 模式的行文本（对齐 Python `"\n".join(texts)`）。
///
/// 表格项的 `text` 本就是展开后的行文本（`table_item`：行内 `,`、行间 `\n`），
/// 与 Python `"\n".join(",".join(r) for r in rows)` 同口径，所以这里只做拼接。
fn flat_text_of(items: &[crate::plugin::RawTextItem]) -> String {
    let mut lines: Vec<&str> = Vec::with_capacity(items.len());
    for it in items {
        if !it.text.is_empty() {
            lines.push(it.text.as_str());
        }
    }
    lines.join("\n")
}

/// 区域部件的读取顺序（对齐 Python `DocxParseCapability._regions` 的 `kind_map`）：
/// 先页眉、再页脚、脚注、尾注，批注排在最后（新增项不移动既有顺序）；同类多部件按 rels 文档序。
const REGION_KINDS: [(&str, &str); 5] = [
    ("header", "header"),
    ("footer", "footer"),
    ("footnotes", "footnote"),
    ("endnotes", "endnote"),
    ("comments", "comment"),
];

/// 读取 `word/header*.xml` / `footer*.xml` / `footnotes.xml` / `endnotes.xml` / `comments.xml`。
///
/// 容错口径与 Python 一致：rels 缺失、部件缺失、单个部件 XML 损坏都只丢掉那一块，
/// 不阻断整篇（区域是附加信息，不该让正文解析失败）。
///
/// 匹配用 `ends_with("/{key}")` 而不是 `contains`：`commentsExtended` / `commentsIds` /
/// `commentsExtensible` 的 Type 都含 `comments` 前缀，但内容模型是 comment id 映射，
/// 没有 `w:p` 正文——`contains` 会把它们当批注部件读一遍（读出来是空，纯浪费）。
fn docx_regions(zip: &mut zip::ZipArchive<std::fs::File>) -> Vec<RawRegion> {
    let Ok(Some(rels)) = zip_read_str(zip, "word/_rels/document.xml.rels") else {
        return Vec::new();
    };
    let rels = super::rel_entries(&rels);

    let mut out: Vec<RawRegion> = Vec::new();
    for (key, kind) in REGION_KINDS {
        let suffix = format!("/{key}");
        for (_id, rtype, target) in &rels {
            if !rtype.to_ascii_lowercase().ends_with(&suffix) {
                continue;
            }
            let part = super::resolve_part("word", target);
            let Ok(Some(xml)) = zip_read_str(zip, &part) else {
                continue;
            };
            let Ok(texts) = region_texts(&xml) else {
                continue;
            };
            out.extend(texts.into_iter().map(|text| RawRegion {
                kind: kind.to_string(),
                text,
            }));
        }
    }
    out
}

/// 区域部件里的段落文本（页眉/页脚/脚注/尾注的内容模型都是 `w:p` 的平铺）。
///
/// 与正文同口径：`w:tab`/`w:br`/`w:cr` 参与占位，文本按 `trim` 过滤空段。
fn region_texts(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().local_name().as_ref() == super::tags::W_P {
                    // extract_para 内部消费到 `</w:p>`，嵌套段（文本框）不会重复计数；
                    // 区域部件不发号（p_start 传 0，字节位就地丢弃）
                    if let Ok((t, _props, _rids, _spans)) =
                        extract_para(&mut reader, &HashMap::new(), 0)
                    {
                        let trimmed = t.trim();
                        if !trimmed.is_empty() {
                            out.push(trimmed.to_string());
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML region parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// 从 `word/document.xml` 的 `<w:body>` 中提取 raw 文本项。
/// 返回按文档序的节点序列：文本项与「该元素内的图片引用」(对齐 Python
/// `items.append(("p", txt))` 之后紧跟 `items.append(("image", im))` 的顺序)。
enum DocxNode {
    /// 段落/表格项
    Item(crate::plugin::RawTextItem),
    /// 紧跟在上一项之后的图片关系 id（一个元素内可能有多个图）
    ImageRids(Vec<String>),
}

/// 追加图片关系 id：同一元素内重复引用只算一次（Python `_blip_rids` 的
/// `if v not in rids`）。去重域是「元素」——段落各自一份，表格整体一份。
fn push_rid(rids: &mut Vec<String>, rid: String) {
    if !rids.contains(&rid) {
        rids.push(rid);
    }
}

fn extend_rids(rids: &mut Vec<String>, more: Vec<String>) {
    for rid in more {
        push_rid(rids, rid);
    }
}

#[cfg_attr(not(test), allow(dead_code))] // 测试包装（β 割接保留），生产路径走 parse_with_spans
fn docx_raw_text(
    xml: &str,
    numbering: &Numbering,
    styles: &HashMap<String, u8>,
) -> Result<Vec<DocxNode>> {
    docx_raw_text_with_slots(xml, numbering, styles)
        .map(|(nodes, _sidecar)| nodes)
}

/// [`docx_raw_text`] 的旁路增强：同一遍扫描顺带发号产 [`DocxSlotSidecar`]
/// （发号规则见类型文档；只加信物/槽位记录，不改任何提取行为）。
fn docx_raw_text_with_slots(
    xml: &str,
    numbering: &Numbering,
    styles: &HashMap<String, u8>,
) -> Result<(Vec<DocxNode>, DocxSlotSidecar)> {
    let mut reader = Reader::from_str(xml);
    // 不裁剪文本：空白由消费者 trim（实体两侧空格需保留，见 GeneralRef 分支）。
    // 旁路槽位表的字节 span 依赖此配置：`buffer_position()` 的事件边界才是
    // 精确字节位（回写侧同一取位法已被既有金测验证）。
    let mut buf = Vec::new();
    let mut nodes: Vec<DocxNode> = Vec::new();
    let mut sidecar = DocxSlotSidecar::default();

    // 找到 <w:body>
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().local_name().as_ref() == super::tags::W_BODY => {
                break
            }
            Ok(Event::Eof) => return Err(DocError::corrupt("no w:body found")),
            Err(e) => return Err(DocError::corrupt(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    buf.clear();

    // 流式处理 body 的直接子级
    loop {
        let pos_before = bpos(&reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().local_name();
                let name = name.as_ref();
                if name == super::tags::W_P {
                    let (t, props, rids, spans) = extract_para(&mut reader, styles, pos_before)?;
                    let trimmed = t.trim().to_string();
                    if !trimmed.is_empty() {
                        let token = sidecar.issue_para(spans, t);
                        let mut item = raw_item(trimmed, &props, numbering);
                        item.source_token = Some(token);
                        nodes.push(DocxNode::Item(item));
                    }
                    if !rids.is_empty() {
                        nodes.push(DocxNode::ImageRids(rids));
                    }
                } else if name == super::tags::W_TBL {
                    let (rows, rids, cells) = extract_table_rows(&mut reader, styles)?;
                    if !rows.is_empty() {
                        let mut item = super::table_item(rows);
                        // 表格项信物 = 其候选格的连续槽位区间（merge 表达格合并；
                        // 全空格表 → None = 不发号，格单元自然无位）
                        item.source_token = sidecar.issue_cells(cells);
                        nodes.push(DocxNode::Item(item));
                    }
                    if !rids.is_empty() {
                        nodes.push(DocxNode::ImageRids(rids));
                    }
                } else if name == super::tags::W_SDT {
                    extract_sdt_items(&mut reader, &mut nodes, numbering, styles, &mut sidecar)?;
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().local_name().as_ref() == super::tags::W_BODY {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    Ok((nodes, sidecar))
}

/// quick-xml 0.42 的 `buffer_position()` 是 `u64`；本模块 span 统一 `usize`。
/// 取位法与回写侧同款：事件读前 = 事件起点、读后 = 事件终点（Reader 未开
/// `trim_text`，位置对空白吞噬免疫）。
fn bpos<R: std::io::BufRead>(reader: &Reader<R>) -> usize {
    reader.buffer_position() as usize
}

// ---------------------------------------------------------------------------
// docforge 补丁：旁路物理槽位表（SourceToken 发号 + 回写字节区间）
// ---------------------------------------------------------------------------

/// 解析顺带记录的段落字节位（[`DocxSlotSidecar`] 的原料，[`extract_para`] 产出）。
#[derive(Debug, Default, Clone)]
struct ParaSpans {
    /// `<w:p` 起符字节偏移（调用方传入的 Start 事件起点）。
    start: usize,
    /// `</w:p>` 闭标签之后的字节偏移。
    end: usize,
    /// `<w:pPr ...>...</w:pPr>` 原文 span（无 pPr / 自闭合 `<w:pPr/>` 时 None）。
    ppr: Option<(usize, usize)>,
    /// 段内 `w:t` 文本内容的字节 span（Text/GeneralRef/CData 事件级；
    /// TranslationOnly 首 `w:t` 换译文、其余清空的定位依据）。
    text_spans: Vec<(usize, usize)>,
    /// 段内各 `w:t` 完整元素字节区间（开标签起符 → 闭标签终符；自闭合空
    /// `<w:t/>` 无内容不入表）。缺陷②超集字段：回写侧快路径的漂移守卫原料
    /// （净荷替换区间必须包住全部 `w:t`，不满足落保真路径）。
    wt_spans: Vec<(usize, usize)>,
    /// 主导 run 直属 `<w:rPr>` 的字节区间（None = 主导 run 无 rPr / 段内无
    /// 参选 run）。选举口径见 [`extract_para`]；`w:pPr` 内的 rPr 结构性不参选
    /// （pPr 子树由 para_props_with 整树消费，不进本循环）。
    dominant_rpr_span: Option<(usize, usize)>,
    /// 段内含非文本内联物件或布局承载元素（[`NON_TEXT_INLINE_TAGS`] 之一，
    /// 或 tab/br/cr/noBreakHyphen）——回写侧 TranslationOnly 据此走保真路径。
    has_non_text_inline: bool,
    /// 段净荷区间：pPr 之后（无 pPr = `<w:p ...>` 开始标签之后）到 `</w:p>`
    /// 起符。快路径整段净荷替换的替换区间。
    /// 段净荷字节区间（pPr 之后 → `</w:p>` 起符；无 pPr = `<w:p ...>` 开始
    /// 标签之后）。**解析侧采集而非回写侧推导**：写入侧从 start 推导净荷起点
    /// 需定位开始标签的 `>`——XML 属性值里合法的 `>`（只禁 `<`/`&`）会把
    /// `find('>')` 骗进标签内部 → Replace 吃掉半个开始标签产坏 XML；解析时
    /// buffer_position 精确无歧义（终点侧 end-5 可导，统一采集保持一致）。
    inner_content: (usize, usize),
}

/// 一个「可翻译段落位」的字节级槽位（与回写侧 `ParaSlot` 消费面同构）。
#[derive(Debug, Clone)]
pub(crate) struct DocxParaSlot {
    /// `<w:p` 起符字节偏移。当前回写消费 `end`/`ppr`/`text_spans`；`start`
    /// 为未来嵌套表写入/段落级重排预留（β 设计的超集字段）。
    #[allow(dead_code)]
    pub(crate) start: usize,
    /// `</w:p>` 之后的字节偏移（BilingualInline 段落插位）。
    pub(crate) end: usize,
    /// 与解析侧 `extract_para` 同口径的段落文本（未 trim；判定用 trim）。
    pub(crate) text: String,
    /// 原段 `<w:pPr ...>...</w:pPr>` 原文 span（无则 None）——复制样式用。
    pub(crate) ppr: Option<(usize, usize)>,
    /// 段内 `w:t` 文本内容字节 span。
    pub(crate) text_spans: Vec<(usize, usize)>,
    /// 段内各 `w:t` 完整元素字节区间（β 超集字段；快路径漂移守卫用，口径见
    /// [`ParaSpans::wt_spans`]）。
    pub(crate) wt_spans: Vec<(usize, usize)>,
    /// 主导 run 直属 `<w:rPr>` 原文 span（None = 主导 run 无 rPr / 无候选；
    /// 快路径/BilingualInline 译文 run 据此切原字节切片）。
    pub(crate) dominant_rpr_span: Option<(usize, usize)>,
    /// 段内含非文本内联物件 → TranslationOnly 保真路径（见 writeback/docx.rs）。
    pub(crate) has_non_text_inline: bool,
    /// 段净荷区间（pPr 之后到 `</w:p>` 起符）——快路径整段替换区间。
    pub(crate) inner_content: (usize, usize),
}

/// 一个「表格格位」的字节级槽位（与回写侧 `CellSlot` 消费面同构 + 格自身 span）。
#[derive(Debug, Clone)]
pub(crate) struct DocxCellSlot {
    /// `<w:tc` 起符字节偏移（β 设计的超集字段，预留未来消费点）。
    #[allow(dead_code)]
    pub(crate) start: usize,
    /// `</w:tc>` 之后的字节偏移（同上预留）。
    #[allow(dead_code)]
    pub(crate) end: usize,
    /// 格文本——解析侧 `extract_table_rows` 单格累积同口径：直接子级 `w:p`
    /// 的非空段（trim）按文档序 join `\n`，嵌套表展平串同位并入；整体 trim。
    pub(crate) text: String,
    /// 格内直接子级 `w:p` 段位（文档序；嵌套表内的段落不在其中）。
    pub(crate) paras: Vec<DocxParaSlot>,
    /// 格内含嵌套 `w:tbl`（v1 回写不写此类格，格位照常占位保对齐）。
    pub(crate) has_nested_table: bool,
}

/// 一个槽位：段落位或格位，按文档序与回写侧旧独立扫描器（`scan_slots`，
/// 已割接删除）的「可翻译位」一一对应。
#[derive(Debug, Clone)]
pub(crate) enum DocxSlot {
    Para(DocxParaSlot),
    Cell(DocxCellSlot),
}

/// docx 旁路物理槽位表：解析一遍顺带产出，回写器按 [`SourceToken`] 区间寻址
/// （`slots[first_slot .. first_slot + slot_count]`）。
///
/// 发号规则 = 解析侧 item 发射序：每个进 items 的 `w:p`（body 直属 / sdt 内）
/// 领 1 个段落槽位；表格 item 的每个候选格（trim 非空 `w:tc`，行主序文档序）
/// 各领 1 个格槽位，item 信物 = 其全部格槽位的连续区间（[`SourceToken::merge`]
/// 表达格合并）。空段 / 空格不占号（与单元流的过滤口径一致）；图片项不发号。
///
/// 注：`</w:body>` 定位等「扫描副产品」不在本表（`extract_sdt_items` 的深度
/// quirk 会吞掉 sdt 尾文档的收尾 End 事件，字节不可靠）——回写侧用一次
/// `rfind("</w:body>")` 轻量定位取（见 writeback/docx.rs），对 quirk 免疫。
#[derive(Debug, Default, Clone)]
pub(crate) struct DocxSlotSidecar {
    pub(crate) slots: Vec<DocxSlot>,
}

impl DocxSlotSidecar {
    /// 发一个段落槽位（`text` 为未 trim 原文），返回该段的单元信物。
    fn issue_para(&mut self, spans: ParaSpans, text: String) -> SourceToken {
        let first_slot = self.slots.len() as u32;
        self.slots.push(DocxSlot::Para(DocxParaSlot {
            start: spans.start,
            end: spans.end,
            text,
            ppr: spans.ppr,
            text_spans: spans.text_spans,
            wt_spans: spans.wt_spans,
            dominant_rpr_span: spans.dominant_rpr_span,
            has_non_text_inline: spans.has_non_text_inline,
            inner_content: spans.inner_content,
        }));
        SourceToken {
            first_slot,
            slot_count: 1,
        }
    }

    /// 发候选格槽位序列（调用方已按 trim 非空过滤），返回表格项信物 = 全部
    /// 格槽位的连续区间（逐格 [`SourceToken::merge`]）。无候选格 / 超出 u16
    /// 计数时返回 None = 不发号（对应单元保守落空位）。
    fn issue_cells(&mut self, cells: Vec<DocxCellSlot>) -> Option<SourceToken> {
        let mut token: Option<SourceToken> = None;
        for cell in cells {
            let one = SourceToken {
                first_slot: self.slots.len() as u32,
                slot_count: 1,
            };
            self.slots.push(DocxSlot::Cell(cell));
            match token.as_mut() {
                Some(t) => t.merge(one),
                None => token = Some(one),
            }
        }
        token
    }
}

/// 跳过 `<w:sdt>` 内部的 `<w:sdtContent>`，提取其中的 `w:p` 段落（顺带发号）。
fn extract_sdt_items<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    nodes: &mut Vec<DocxNode>,
    numbering: &Numbering,
    styles: &HashMap<String, u8>,
    sidecar: &mut DocxSlotSidecar,
) -> Result<()> {
    let mut buf = Vec::new();
    let mut depth = 1;
    loop {
        let pos_before = bpos(reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                depth += 1;
                if e.name().local_name().as_ref() == super::tags::W_P {
                    let (t, props, rids, spans) = extract_para(reader, styles, pos_before)?;
                    let trimmed = t.trim().to_string();
                    if !trimmed.is_empty() {
                        let token = sidecar.issue_para(spans, t);
                        let mut item = raw_item(trimmed, &props, numbering);
                        item.source_token = Some(token);
                        nodes.push(DocxNode::Item(item));
                    }
                    if !rids.is_empty() {
                        nodes.push(DocxNode::ImageRids(rids));
                    }
                }
            }
            Ok(Event::End(ref _e)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML sdt parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// 主导 rPr 选举的单 run 结算（缺陷②）：run 带可见字符且不含 `w:vertAlign`
/// （上下标 run 不参选——段首上标角标把样式劫持给整段译文正是要修的缺陷）时
/// 入选；字符数严格大于当前 Winner 才替换（平手取文档序先者，确定性口径）。
/// 无 rPr 的 run 同样参选：它胜出时 Winner 的 rPr = None → 快路径译文 run
/// 不带 rPr 切片（口径：缺省样式即主导样式，见 writeback/docx.rs）。
fn elect_run(
    best: &mut Option<(usize, Option<(usize, usize)>)>,
    chars: usize,
    rpr: Option<(usize, usize)>,
    vert: bool,
) {
    let wins = match best {
        Some((c, _)) => chars > *c,
        None => true,
    };
    if chars > 0 && !vert && wins {
        *best = Some((chars, rpr));
    }
}

/// 消费整个子树直到与开元素配对的 End 事件（缺陷①：`<mc:Fallback>` 整树跳过）。
/// 深度计数口径：Start +1 / End -1，进入时置 1、清零恢复；自闭合 Empty 不进
/// 本函数（调用方 Start 分支才匹配），天然免疫。与回写侧 `consume_subtree`
/// 同构，与 para_props 同款「整树消费、主循环深度不增减」；Eof 兜底（病态
/// 截断不悬死）。
fn skip_subtree<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<()> {
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
            Err(e) => return Err(DocError::corrupt(format!("XML fallback skip error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
}

/// 提取 `<w:p>` 段落的文本与段落属性，处理 w:pPr / w:r / w:t / w:tab / w:br / w:cr。
/// 调用时 reader 刚读过 `<w:p>` Start 事件（`p_start` = 该 Start 的字节起点），
/// 退出时 reader 处于 `</w:p>` End 事件之后。
/// 第三个返回值是段落内出现的图片关系 id（`a:blip` / `v:imagedata`，文档序）；
/// 第四个是段落字节位（旁路槽位表原料，不消费者可丢弃）。
///
/// 缺陷①：`<mc:Fallback>` 子树整树跳过（[`skip_subtree`]）——文本、span、
/// rid、占位符同步跳过，只提取 Choice 分支。
/// 缺陷②采集（同一遍顺带，不改文本行为）：主导 rPr 选举（见 [`elect_run`]；
/// 采样 = run 直属 rPr + `w:t` 内解码字符数；`w:pPr` 内的 rPr 由 para_props
/// 整树消费、结构性不参选——「只认 run 直属」）、`w:t` 全元素区间、段净荷
/// 区间、非文本内联物件判据（[`NON_TEXT_INLINE_TAGS`] + tab/br/cr/noBreakHyphen，
/// br / tab 是文本占位不算物件）。
///
/// 嵌套说明：文本框等嵌套 `w:p` 的 `w:t` 照旧并入外层段落文本（不重复计数）；
/// 选举用单 run 上下文（不深跟踪嵌套 run）——含文本框/图段的段落必带
/// `has_non_text_inline` 走回写保真路径、选举结果不被消费，而合法 OOXML 中
/// 快路径段落（无内联物件）不存在嵌套 run，单上下文即精确。
fn extract_para<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    styles: &HashMap<String, u8>,
    p_start: usize,
) -> Result<(String, ParaProps, Vec<String>, ParaSpans)> {
    let mut out = String::new();
    let mut props = ParaProps::default();
    let mut buf = Vec::new();
    let mut depth = 1;
    let mut rids: Vec<String> = Vec::new();
    let mut spans = ParaSpans {
        start: p_start,
        ..Default::default()
    };
    // 只在 `w:t` 内收文本：段落里的缩进空白是 Text 事件，但属于 XML 格式而非正文
    // （Python `_docx_para_text` 也只取 `w:t` 文本，且 `w:t` 内的空白是有意义的）
    let mut in_t = false;
    // 净荷起点 = `<w:p ...>` 开始标签之后（调用方刚消费 Start 事件，此位即标签
    // 终点）；pPr 消费后改记 pPr 终点（快路径替换区间 = pPr 之后到 `</w:p>` 之前）
    let mut inner_start = bpos(reader);
    // 当前 `w:t` 开标签起符（wt_spans 记完整元素区间）
    let mut wt_open: Option<usize> = None;
    // 主导选举的当前 run 上下文（口径见函数文档）
    let mut in_run = false;
    let mut run_rpr: Option<(usize, usize)> = None;
    let mut run_rpr_open: Option<usize> = None;
    let mut run_chars = 0usize;
    let mut run_vert = false;
    // 选举 Winner：(可见字符数, 该 run 直属 rPr span)
    let mut best: Option<(usize, Option<(usize, usize)>)> = None;

    loop {
        let pos_before = bpos(reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let local = e.name().local_name();
                let local = local.as_ref();
                // 同一元素内的重复引用只算一次（Python `_blip_rids`：`v not in rids`）
                if let Some(rid) = super::image::rid_of_image_node(e) {
                    push_rid(&mut rids, rid);
                }
                if NON_TEXT_INLINE_TAGS.contains(&local) {
                    spans.has_non_text_inline = true;
                }
                match local {
                    // 段落属性在跑文本之前：由 para_props 整个消费掉，深度不增减
                    super::tags::W_PPR => {
                        let ppr_start = pos_before;
                        props = para_props_with(reader, styles)?;
                        spans.ppr = Some((ppr_start, bpos(reader)));
                        // 净荷起点改记 pPr 终点（快路径净荷 = pPr 之后）
                        inner_start = bpos(reader);
                    }
                    // 缺陷①：Fallback 是 Choice 的同内容 VML 镜像，整树跳过
                    // （Enter 置 1 / 配对清零由整树消费等价实现，主循环深度不增减）
                    TAG_FALLBACK => skip_subtree(reader)?,
                    super::tags::W_T => {
                        in_t = true;
                        wt_open = Some(pos_before);
                        depth += 1;
                    }
                    super::tags::W_R => {
                        // run 边界：结算上一 run（合法文档 run 不嵌套，此处只兜
                        // 病态悬空；同款重置见 `</w:r>` 分支）
                        elect_run(&mut best, run_chars, run_rpr, run_vert);
                        in_run = true;
                        run_rpr = None;
                        run_rpr_open = None;
                        run_chars = 0;
                        run_vert = false;
                        depth += 1;
                    }
                    TAG_RPR => {
                        // 只认 run 直属 rPr（pPr 内 rPr 已整树消费，不进本循环）
                        if in_run {
                            run_rpr_open = Some(pos_before);
                        }
                        depth += 1;
                    }
                    TAG_VERT_ALIGN => {
                        if in_run {
                            run_vert = true;
                        }
                        depth += 1;
                    }
                    super::tags::W_TAB => {
                        depth += 1;
                        // tab/br/cr/noBreakHyphen 是**布局承载元素**：旧写入路径
                        // 保留这些 run（非 w:t 不清空），快路径整段净荷替换会把
                        // 它们删掉——软换行段落变单行、tab 位/分页符丢失。凡提取
                        // 为 \t/\n/- 占位的都触发保真路径（含分页 br）。
                        spans.has_non_text_inline = true;
                        out.push('\t');
                    }
                    super::tags::W_BR | super::tags::W_CR => {
                        depth += 1;
                        spans.has_non_text_inline = true; // 同上：布局承载
                        out.push('\n');
                    }
                    super::tags::W_NBH => {
                        depth += 1;
                        spans.has_non_text_inline = true; // 同上
                        out.push('-');
                    }
                    _ => depth += 1,
                }
            }
            Ok(Event::End(ref e)) => {
                let local = e.name().local_name();
                let local = local.as_ref();
                if local == super::tags::W_T {
                    in_t = false;
                    if let Some(open) = wt_open.take() {
                        spans.wt_spans.push((open, bpos(reader)));
                    }
                } else if local == TAG_RPR {
                    if let Some(s) = run_rpr_open.take() {
                        run_rpr = Some((s, bpos(reader)));
                    }
                } else if local == super::tags::W_R {
                    elect_run(&mut best, run_chars, run_rpr, run_vert);
                    in_run = false;
                    run_rpr = None;
                    run_rpr_open = None;
                    run_chars = 0;
                    run_vert = false;
                }
                depth -= 1;
                if depth == 0 {
                    // `</w:p>` 起符 = 净荷终点（不含闭标签）
                    spans.inner_content = (inner_start, pos_before);
                    break;
                }
            }
            Ok(Event::Empty(ref e)) => {
                // `<a:blip r:embed="rId1"/>` 这种自闭合图片引用
                if let Some(rid) = super::image::rid_of_image_node(e) {
                    push_rid(&mut rids, rid);
                }
                let local = e.name().local_name();
                let local = local.as_ref();
                if NON_TEXT_INLINE_TAGS.contains(&local) {
                    spans.has_non_text_inline = true;
                }
                match local {
                    // 布局承载元素（Start 分支同款注释）：全部触发保真路径
                    super::tags::W_TAB => {
                        spans.has_non_text_inline = true;
                        out.push('\t');
                    }
                    super::tags::W_BR | super::tags::W_CR => {
                        spans.has_non_text_inline = true;
                        out.push('\n');
                    }
                    // 不间断连字符（Python `W_NBH` → "-"）
                    super::tags::W_NBH => {
                        spans.has_non_text_inline = true;
                        out.push('-');
                    }
                    TAG_VERT_ALIGN => {
                        if in_run {
                            run_vert = true;
                        }
                    }
                    TAG_RPR
                        // 自闭合空 rPr：切片 = `<w:rPr/>` 本身（合法可嵌回）
                        if in_run && run_rpr.is_none() && run_rpr_open.is_none() => {
                            run_rpr = Some((pos_before, bpos(reader)));
                        }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_t {
                    let span_start = pos_before;
                    // quick-xml 0.38+：Text 已是解码后的字符串，实体需显式 unescape
                    let raw = e.xml10_content();
                    if let Ok(t) = quick_xml::escape::unescape(&raw) {
                        out.push_str(&t);
                        // 主导选举：run 内可见字符计数（解码后口径）
                        run_chars += t.chars().count();
                    }
                    spans.text_spans.push((span_start, bpos(reader)));
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if in_t {
                    let span_start = pos_before;
                    super::push_general_ref(&mut out, e);
                    // 命名实体至多展开为短串，按一个可见字形计（选举只比相对占比）
                    run_chars += 1;
                    spans.text_spans.push((span_start, bpos(reader)));
                }
            }
            Ok(Event::CData(ref e)) => {
                if in_t {
                    let span_start = pos_before;
                    // CDATA 内容不含实体，直接取字符串
                    out.push_str(e.as_ref());
                    run_chars += e.as_ref().chars().count();
                    spans.text_spans.push((span_start, bpos(reader)));
                }
            }
            Err(e) => return Err(DocError::corrupt(format!("XML para parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    // 段尾悬空 run（病态输入）的防御性结算；正常文档 `</w:r>` 先于 `</w:p>` 闭合
    elect_run(&mut best, run_chars, run_rpr, run_vert);
    spans.dominant_rpr_span = best.and_then(|(_, rpr)| rpr);
    // Eof 兜底（无 `</w:p>` 的病态截断）：净荷终点随段终点
    if spans.inner_content.1 == 0 {
        spans.inner_content.1 = bpos(reader);
    }
    spans.end = bpos(reader);
    Ok((out, props, rids, spans))
}

/// 段落文本 + 段落属性 → raw 项。
///
/// 标题优先于列表（对齐 Python `_para`：先判标题，标题命中就不再产出 list）。
fn raw_item(
    text: String,
    props: &ParaProps,
    numbering: &Numbering,
) -> crate::plugin::RawTextItem {
    crate::plugin::RawTextItem {
        text,
        level: props.level,
        list: props.list_ordered(numbering),
        ..Default::default()
    }
}

/// 段落属性（对标 Python `_docx_para_level` + `_docx_numpr`）。
#[derive(Debug, Default, Clone)]
struct ParaProps {
    /// 标题层级（1 基）
    level: Option<u8>,
    /// `w:numPr` → (numId, ilvl)；numId 缺省 ""、ilvl 缺省 "0"（与 Python 同口径）
    num: Option<(String, String)>,
}

impl ParaProps {
    /// 列表有序性：标题段落不进列表（返回 None）；无 `w:numPr` 返回 None（普通段落）。
    fn list_ordered(&self, numbering: &Numbering) -> Option<bool> {
        if self.level.is_some() {
            return None;
        }
        self.num
            .as_ref()
            .map(|(num_id, ilvl)| numbering.is_ordered(num_id, ilvl))
    }
}

/// `<w:pPr>` 子树扫描状态。
#[derive(Debug, Default)]
struct PPrScan {
    outline: Option<u8>,
    style_id: Option<String>,
    num_id: Option<String>,
    ilvl: Option<String>,
    /// 是否处于 `w:numPr` 内（`numId` / `ilvl` 只在该子树里才认）
    in_numpr: bool,
    /// 是否出现过 `w:numPr`（自闭合空 `w:numPr` 也算，与 Python 一致）
    saw_numpr: bool,
}

impl PPrScan {
    fn on_start(&mut self, e: &quick_xml::events::BytesStart<'_>) {
        let name = e.name();
        let name = name.local_name();
        match name.as_ref() {
            super::tags::W_PSTYLE => self.style_id = attr_val(e),
            super::tags::W_OUTLINE_LVL => {
                // Python：int(val)，只接受 0..=8；越界/非法一律不认
                if let Some(v) = attr_val(e) {
                    self.outline = v
                        .trim()
                        .parse::<u8>()
                        .ok()
                        .filter(|n| *n < 9)
                        .map(|n| n + 1);
                }
            }
            super::tags::W_NUMPR => {
                self.saw_numpr = true;
                self.in_numpr = true;
            }
            super::tags::W_NUMID if self.in_numpr => {
                self.num_id = Some(attr_val(e).unwrap_or_default());
            }
            super::tags::W_ILVL if self.in_numpr => {
                self.ilvl = Some(attr_val(e).unwrap_or_else(|| "0".to_string()));
            }
            _ => {}
        }
    }

    fn on_empty(&mut self, e: &quick_xml::events::BytesStart<'_>) {
        // 常见写法都是自闭合（<w:pStyle/> <w:outlineLvl/> <w:numId/> <w:numPr/>）
        self.on_start(e);
        if e.name().local_name().as_ref() == super::tags::W_NUMPR {
            // 自闭合的 numPr 没有子树，立刻退出「在 numPr 内」状态
            self.in_numpr = false;
        }
    }

    fn on_end(&mut self, e: &quick_xml::events::BytesEnd<'_>) {
        if e.name().local_name().as_ref() == super::tags::W_NUMPR {
            self.in_numpr = false;
        }
    }

    fn into_props_with(self, styles: &HashMap<String, u8>) -> ParaProps {
        // 与 Python `_docx_para_level` 同序：pStyle 命中优先，否则看 outlineLvl。
        // docforge 补丁：styleId 直接判定不中时查 styles.xml 的 name 映射
        //（zh-CN 数字 styleId → "heading N"）。
        let level = self
            .style_id
            .as_deref()
            .and_then(heading_level_from_style)
            .or_else(|| self.style_id.as_deref().and_then(|id| styles.get(id).copied()))
            .or(self.outline);
        let num = self.saw_numpr.then(|| {
            (
                self.num_id.clone().unwrap_or_default(),
                self.ilvl.clone().unwrap_or_else(|| "0".to_string()),
            )
        });
        ParaProps { level, num }
    }
}

/// 解析 `<w:pPr>` 子树：标题层级（`w:pStyle` / `w:outlineLvl`）与编号（`w:numPr`）。
///
/// 调用时 reader 刚读过 `<w:pPr>` Start，退出时处于 `</w:pPr>` End 之后。
fn para_props_with<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    styles: &HashMap<String, u8>,
) -> Result<ParaProps> {
    let mut scan = PPrScan::default();
    let mut buf = Vec::new();
    let mut depth = 1usize;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                scan.on_start(e);
                depth += 1;
            }
            Ok(Event::Empty(ref e)) => scan.on_empty(e),
            Ok(Event::End(ref e)) => {
                scan.on_end(e);
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML pPr parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    Ok(scan.into_props_with(styles))
}

/// `word/numbering.xml`：`numId` + `ilvl` → 是否有序（`bullet` / `none` 视为无序）。
///
/// 对齐 Python `_DocxNumbering`：`w:abstractNum` 提供每级 `w:numFmt`，
/// `w:num` 把 `numId` 映射到 `abstractNumId`；未定义/未知 ilvl 一律按无序。
/// 解析失败不阻断整篇（空表 = 全部无序）。
#[derive(Debug, Default)]
struct Numbering {
    ordered: HashMap<String, HashMap<String, bool>>,
}

impl Numbering {
    fn parse(xml: &str) -> Self {
        let mut scan = NumberingScan::default();
        let mut reader = Reader::from_str(xml);
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => scan.on_start(e),
                Ok(Event::Empty(ref e)) => scan.on_empty(e),
                Ok(Event::End(ref e)) => scan.on_end(e),
                Ok(Event::Eof) => break,
                // 损坏的 numbering.xml：保留已解析部分（Python 同样 except 后返回空表）
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        scan.finish()
    }

    fn is_ordered(&self, num_id: &str, ilvl: &str) -> bool {
        self.ordered
            .get(num_id)
            .and_then(|levels| levels.get(ilvl))
            .copied()
            .unwrap_or(false)
    }
}

/// `word/numbering.xml` 扫描状态。
#[derive(Debug, Default)]
struct NumberingScan {
    /// abstractNumId → (ilvl → numFmt)
    abstract_fmts: HashMap<String, HashMap<String, String>>,
    /// (numId, abstractNumId)
    nums: Vec<(String, String)>,
    cur_aid: Option<String>,
    cur_ilvl: Option<String>,
    cur_num: Option<String>,
}

impl NumberingScan {
    fn on_start(&mut self, e: &quick_xml::events::BytesStart<'_>) {
        let name = e.name();
        let name = name.local_name();
        self.on_leaf(e);
        match name.as_ref() {
            super::tags::W_ABSTRACTNUM => {
                self.cur_aid = Some(
                    attr_by(e, super::tags::W_ABSTRACTNUMID).unwrap_or_else(|| "0".to_string()),
                )
            }
            super::tags::W_LVL => {
                self.cur_ilvl =
                    Some(attr_by(e, super::tags::W_ILVL).unwrap_or_else(|| "0".to_string()))
            }
            super::tags::W_NUM => {
                self.cur_num = Some(attr_by(e, super::tags::W_NUMID).unwrap_or_default())
            }
            _ => {}
        }
    }

    fn on_empty(&mut self, e: &quick_xml::events::BytesStart<'_>) {
        self.on_leaf(e);
    }

    fn on_end(&mut self, e: &quick_xml::events::BytesEnd<'_>) {
        match e.name().local_name().as_ref() {
            super::tags::W_ABSTRACTNUM => self.cur_aid = None,
            super::tags::W_LVL => self.cur_ilvl = None,
            super::tags::W_NUM => self.cur_num = None,
            _ => {}
        }
    }

    /// 叶子元素（成对或自闭合都可能）：`w:numFmt` 记格式，`w:abstractNumId` 记 num→abstract 映射。
    fn on_leaf(&mut self, e: &quick_xml::events::BytesStart<'_>) {
        let name = e.name();
        let name = name.local_name();
        match name.as_ref() {
            super::tags::W_NUMFMT => {
                if let (Some(aid), Some(ilvl)) = (self.cur_aid.as_ref(), self.cur_ilvl.as_ref()) {
                    let fmt = attr_val(e).unwrap_or_else(|| "bullet".to_string());
                    self.abstract_fmts
                        .entry(aid.clone())
                        .or_default()
                        .insert(ilvl.clone(), fmt);
                }
            }
            super::tags::W_ABSTRACTNUMID => {
                if let Some(num_id) = self.cur_num.as_ref() {
                    self.nums
                        .push((num_id.clone(), attr_val(e).unwrap_or_default()));
                }
            }
            _ => {}
        }
    }

    fn finish(self) -> Numbering {
        let mut ordered: HashMap<String, HashMap<String, bool>> = HashMap::new();
        for (num_id, aid) in self.nums {
            let levels = self
                .abstract_fmts
                .get(&aid)
                .map(|fmts| {
                    fmts.iter()
                        .map(|(ilvl, fmt)| (ilvl.clone(), fmt != "bullet" && fmt != "none"))
                        .collect()
                })
                .unwrap_or_default();
            ordered.insert(num_id, levels);
        }
        Numbering { ordered }
    }
}

/// 按本地名取属性值（`w:val` 等）。
fn attr_val(e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    attr_by(e, super::tags::VAL)
}

/// `word/styles.xml` 的 styleId → 标题层级映射（docforge M2 收尾补丁）。
///
/// zh-CN Word 常用**数字 styleId**（`<w:pStyle w:val="1"/>`，样式名才是
/// `heading 1`）——只看 styleId 会漏判。此表以 `<w:name w:val>` 判定，
/// 与 [`heading_level_from_style`] 同语义。解析失败按空表处理（不阻断）。
fn parse_styles_heading_map(xml: &str) -> HashMap<String, u8> {
    let mut map = HashMap::new();
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut cur_id: Option<String> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local_name = e.name().local_name();
                let local = local_name.as_ref();
                if local == super::tags::W_PSTYLE || local == "style" {
                    // style 声明元素（w:style）；pStyle 不会出现在 styles.xml 顶层，双保险跳过
                    if let Some(id) = attr_by(e, "styleId") {
                        if local == "style" {
                            cur_id = Some(id);
                        }
                    }
                } else if local == "name" {
                    if let (Some(id), Some(name)) = (cur_id.clone(), attr_val(e)) {
                        if let Some(level) = heading_level_from_style(&name) {
                            map.insert(id, level);
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "style" => {
                cur_id = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    map
}

/// 从段落样式 id 推断标题级别，语义对齐 Python `_docx_para_level`：
/// 样式名含 `heading`（大小写无关）或 `标题` 才算标题，级别取其中第一段数字，取不到按 1 级。
fn heading_level_from_style(id: &str) -> Option<u8> {
    let is_heading = id.to_ascii_lowercase().contains("heading") || id.contains("标题");
    if !is_heading {
        return None;
    }
    let digits: String = id.chars().filter(|c| c.is_ascii_digit()).collect();
    let n = digits.parse::<u8>().ok().unwrap_or(1);
    Some(n.clamp(1, 9))
}

/// `extract_table_rows` 的返回形态（clippy type_complexity 别名）。
type TableRowsWithSlots = (Vec<Vec<String>>, Vec<String>, Vec<DocxCellSlot>);

/// 提取 `<w:tbl>` 的行列结构，处理嵌套表；顺带记录本表**直接格**的候选格
/// 槽位（trim 非空 `w:tc`，文档序——嵌套表的格由递归消费、就地丢弃，其段落
/// 已按解析侧口径展平进所在格文本）。
/// 调用时 reader 刚读过 `<w:tbl>` Start 事件，退出时 reader 处于 `</w:tbl>` End 事件之后。
fn extract_table_rows<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    styles: &HashMap<String, u8>,
) -> Result<TableRowsWithSlots> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_cell = String::new();
    let mut tbl_depth = 1;
    let mut buf = Vec::new();
    let mut cell_has_content = false;
    // 表格里的图片（Python `_walk` 对 `w:tbl` 元素整体扫 `_blip_rids`）
    let mut rids: Vec<String> = Vec::new();
    // 本表直接格的槽位原料（与文本累积同序同判定）
    let mut cells: Vec<DocxCellSlot> = Vec::new();
    let mut tc_start = 0usize;
    let mut cell_paras: Vec<DocxParaSlot> = Vec::new();
    let mut cell_has_nested = false;

    loop {
        let pos_before = bpos(reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                tbl_depth += 1;
                if let Some(rid) = super::image::rid_of_image_node(e) {
                    push_rid(&mut rids, rid);
                }
                match e.name().local_name().as_ref() {
                    super::tags::W_P => {
                        // extract_para 在内部消费了 </w:p>，需补偿深度；
                        // 单元格内的段落不参与标题/列表判定（只在正文层有意义）
                        let (t, _props, inner_rids, spans) =
                            extract_para(reader, styles, pos_before)?;
                        tbl_depth -= 1;
                        extend_rids(&mut rids, inner_rids);
                        if !t.trim().is_empty() {
                            if cell_has_content {
                                current_cell.push('\n');
                            }
                            current_cell.push_str(t.trim());
                            cell_has_content = true;
                        }
                        // 段位照常收（写入用；与回写侧旧扫描同口径：不按 trim 过滤）
                        cell_paras.push(DocxParaSlot {
                            start: spans.start,
                            end: spans.end,
                            text: t,
                            ppr: spans.ppr,
                            text_spans: spans.text_spans,
                            wt_spans: spans.wt_spans,
                            dominant_rpr_span: spans.dominant_rpr_span,
                            has_non_text_inline: spans.has_non_text_inline,
                            inner_content: spans.inner_content,
                        });
                        continue;
                    }
                    super::tags::W_TBL => {
                        // 嵌套表：递归提取，其内部也消费了 </w:tbl>（其格槽位
                        // 就地丢弃——嵌套段落已展平进所在格文本，不是独立位）
                        let (nested, nested_rids, _nested_cells) =
                            extract_table_rows(reader, styles)?;
                        tbl_depth -= 1;
                        extend_rids(&mut rids, nested_rids);
                        cell_has_nested = true;
                        if !nested.is_empty() {
                            if cell_has_content {
                                current_cell.push('\n');
                            }
                            current_cell.push_str(
                                &nested
                                    .iter()
                                    .map(|r| r.join(","))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            );
                            cell_has_content = true;
                        }
                        continue;
                    }
                    super::tags::W_TC => tc_start = pos_before,
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                if let Some(rid) = super::image::rid_of_image_node(e) {
                    push_rid(&mut rids, rid);
                }
            }
            Ok(Event::End(ref e)) => {
                tbl_depth -= 1;
                match e.name().local_name().as_ref() {
                    super::tags::W_TBL => {
                        if tbl_depth == 0 {
                            if !current_row.is_empty() {
                                rows.push(std::mem::take(&mut current_row));
                            }
                            break;
                        }
                    }
                    super::tags::W_TR => {
                        // 行结束：单元格已由 W_TC End 处理，只需保存行
                        if !current_row.is_empty() {
                            rows.push(std::mem::take(&mut current_row));
                        }
                    }
                    super::tags::W_TC => {
                        // 单元格结束：保存内容
                        if cell_has_content {
                            current_row.push(current_cell.trim().to_string());
                        } else {
                            current_row.push(String::new());
                        }
                        // 候选格（trim 非空）登记槽位；空格丢弃（单元流同样不发）
                        let text = current_cell.trim().to_string();
                        if !text.is_empty() {
                            cells.push(DocxCellSlot {
                                start: tc_start,
                                end: bpos(reader),
                                text,
                                paras: std::mem::take(&mut cell_paras),
                                has_nested_table: cell_has_nested,
                            });
                        }
                        cell_paras.clear();
                        cell_has_nested = false;
                        current_cell.clear();
                        cell_has_content = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML table parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok((rows, rids, cells))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无 numbering.xml 的提取（带 `w:numPr` 的段落一律无序列表）。
    /// 图片节点在这里被丢弃（需要 ZIP 才能解出字节）。
    fn parse_items(xml: &str) -> Vec<crate::plugin::RawTextItem> {
        docx_raw_text(xml, &Numbering::default(), &HashMap::new())
            .unwrap()
            .into_iter()
            .filter_map(|n| match n {
                DocxNode::Item(it) => Some(it),
                DocxNode::ImageRids(_) => None,
            })
            .collect()
    }

    /// 文档序的图片 rid 列表（跳过文本项）。
    fn parse_rids(xml: &str) -> Vec<String> {
        docx_raw_text(xml, &Numbering::default(), &HashMap::new())
            .unwrap()
            .into_iter()
            .flat_map(|n| match n {
                DocxNode::ImageRids(rids) => rids,
                DocxNode::Item(_) => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn heading_by_outline_level() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:pPr><w:outlineLvl w:val="0"/></w:pPr><w:r><w:t>一级标题</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:outlineLvl w:val="2"/></w:pPr><w:r><w:t>三级标题</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>正文</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items[0].level, Some(1)); // outlineLvl 0 基 → 1 基
        assert_eq!(items[1].level, Some(3));
        assert_eq!(items[2].level, None);
    }

    #[test]
    fn heading_by_paragraph_style() {
        // 与 Python `_docx_para_level` 对齐："heading"/"标题" 命中才算标题，取第一段数字
        for (style, want) in [
            ("Heading1", Some(1)),
            ("heading2", Some(2)),
            ("标题3", Some(3)),
            ("标题", Some(1)),
            ("HeadingX", Some(1)),
            ("heading-4", Some(4)),
            ("Normal", None),
            ("4", None),
        ] {
            assert_eq!(heading_level_from_style(style), want, "style={style}");
        }
    }

    #[test]
    fn paragraph_style_flows_through_document() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>章节</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items[0].text, "章节");
        assert_eq!(items[0].level, Some(2));
    }

    /// 与 Python fixtures 同构的 numbering.xml：numId 1 → decimal，numId 2 → bullet。
    const NUMBERING_XML: &str = concat!(
        r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
        r#"<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl></w:abstractNum>"#,
        r#"<w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum>"#,
        r#"<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
        r#"<w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>"#,
        r#"</w:numbering>"#
    );

    #[test]
    fn numbering_resolves_numfmt() {
        let n = Numbering::parse(NUMBERING_XML);
        assert!(n.is_ordered("1", "0"), "decimal → 有序");
        assert!(!n.is_ordered("2", "0"), "bullet → 无序");
        assert!(!n.is_ordered("9", "0"), "未定义 numId → 无序");
        assert!(!n.is_ordered("2", "1"), "未定义 ilvl → 无序");
    }

    #[test]
    fn numbering_missing_numfmt_defaults_bullet() {
        let xml = concat!(
            r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"/></w:abstractNum>"#,
            r#"<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>"#,
            r#"</w:numbering>"#
        );
        // Python：无 numFmt 视作 "bullet"
        assert!(!Numbering::parse(xml).is_ordered("1", "0"));
    }

    #[test]
    fn numpr_paragraph_becomes_list() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>编号第一项</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>要点A</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>正文</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let numbering = Numbering::parse(NUMBERING_XML);
        let items = docx_raw_text(xml, &numbering, &HashMap::new())
            .unwrap()
            .into_iter()
            .filter_map(|n| match n {
                DocxNode::Item(it) => Some(it),
                DocxNode::ImageRids(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(items[0].list, Some(true)); // decimal
        assert_eq!(items[0].level, None);
        assert_eq!(items[1].list, Some(false)); // bullet
        assert_eq!(items[2].list, None); // 普通段落
    }

    /// numbering.xml 缺失：带 numPr 仍是列表，但一律无序（对齐 Python `numbering is None`）。
    #[test]
    fn numpr_without_numbering_is_unordered() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>项</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items[0].list, Some(false));
    }

    /// 标题优先：标题段落即使带 numPr 也只算标题（对齐 Python `_para` 的判定顺序）。
    #[test]
    fn heading_wins_over_numpr() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading2"/><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>带编号的标题目录</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let numbering = Numbering::parse(NUMBERING_XML);
        let items = docx_raw_text(xml, &numbering, &HashMap::new())
            .unwrap()
            .into_iter()
            .filter_map(|n| match n {
                DocxNode::Item(it) => Some(it),
                DocxNode::ImageRids(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(items[0].level, Some(2));
        assert_eq!(items[0].list, None);
    }

    /// 空 `<w:numPr/>`（无 numId）：Python 仍判为列表，无序。
    #[test]
    fn empty_numpr_is_unordered_list() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:pPr><w:numPr/></w:pPr><w:r><w:t>项</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items[0].list, Some(false));
    }

    /// 表格单元格内的段落不参与列表判定（单元格只保留文本）。
    #[test]
    fn table_cell_numpr_is_not_list() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:tbl><w:tr><w:tc><w:p><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>格</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
            r#"</w:body>"#
        );
        let numbering = Numbering::parse(NUMBERING_XML);
        let items = docx_raw_text(xml, &numbering, &HashMap::new())
            .unwrap()
            .into_iter()
            .filter_map(|n| match n {
                DocxNode::Item(it) => Some(it),
                DocxNode::ImageRids(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(items[0].text, "格");
        assert_eq!(items[0].list, None);
        assert_eq!(items[0].level, None);
    }

    /// pPr 里带 rPr 等非标题子节点时，深度必须平衡（不能提前跳出段落）。
    #[test]
    fn ppr_with_nested_props_keeps_text() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:pPr><w:rPr><w:b/></w:rPr><w:jc w:val="center"/></w:pPr>"#,
            r#"<w:r><w:t>居中正文</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>下一段</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "居中正文");
        assert_eq!(items[0].level, None);
        assert_eq!(items[1].text, "下一段");
    }

    #[test]
    fn test_para_simple() {
        let xml = r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:p><w:r><w:t>Hello</w:t></w:r></w:p>
</w:body>"#;
        let items = parse_items(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Hello");
    }

    #[test]
    fn test_para_tab_br() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"  <w:p><w:r><w:t>a</w:t></w:r><w:tab/><w:r><w:t>b</w:t></w:r><w:br/><w:r><w:t>c</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "a\tb\nc");
    }

    #[test]
    fn test_simple_table() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"  <w:tbl>"#,
            r#"    <w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr>"#,
            r#"  </w:tbl>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "A,B"); // 展开文本（页文本层）
        let table = items[0].table.as_ref().expect("表格项应带行列结构");
        assert_eq!(table.rows, vec![vec!["A".to_string(), "B".to_string()]]);
        assert_eq!(items[0].level, None); // 表格不是标题
        assert_eq!(items[0].list, None);
    }

    #[test]
    fn test_nested_table() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"  <w:tbl>"#,
            r#"    <w:tr>"#,
            r#"      <w:tc><w:p><w:r><w:t>X</w:t></w:r></w:p></w:tc>"#,
            r#"      <w:tc>"#,
            r#"        <w:p><w:r><w:t>Y</w:t></w:r></w:p>"#,
            r#"        <w:tbl>"#,
            r#"          <w:tr><w:tc><w:p><w:r><w:t>innerA</w:t></w:r></w:p></w:tc>"#,
            r#"               <w:tc><w:p><w:r><w:t>innerB</w:t></w:r></w:p></w:tc></w:tr>"#,
            r#"        </w:tbl>"#,
            r#"      </w:tc>"#,
            r#"    </w:tr>"#,
            r#"  </w:tbl>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items.len(), 1);
        assert!(items[0].text.contains("innerA"));
        assert!(items[0].text.contains("innerB"));
        // 嵌套表按 Python `_docx_cell_text` 口径并入单元格文本（不产出第二个表格项）
        let rows = &items[0].table.as_ref().unwrap().rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "X");
        assert_eq!(rows[0][1], "Y\ninnerA,innerB");
    }

    /// 段落与表格混排时顺序保持（表格项夹在段落项之间）。
    #[test]
    fn table_keeps_document_order() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:t>前</w:t></w:r></w:p>"#,
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
            r#"<w:p><w:r><w:t>后</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].text, "前");
        assert!(items[1].table.is_some());
        assert_eq!(items[2].text, "后");
    }

    /// `w:t` 内的空白有意义（Python 逐 `w:t` 取文本）；XML 缩进空白不进正文。
    #[test]
    fn wt_whitespace_is_meaningful() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            "\n  ",
            r#"<w:p>"#,
            "\n    ",
            r#"<w:r><w:t>Hello</w:t></w:r>"#,
            "\n    ",
            r#"<w:r><w:t xml:space="preserve"> </w:t></w:r>"#,
            "\n    ",
            r#"<w:r><w:t>World</w:t></w:r>"#,
            "\n  ",
            r#"</w:p>"#,
            "\n",
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Hello World");
    }

    /// 不间断连字符 → `-`（Python `_docx_para_text` 的 W_NBH 分支）。
    #[test]
    fn no_break_hyphen_renders_dash() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:t>anti</w:t><w:noBreakHyphen/><w:t>virus</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items[0].text, "anti-virus");
    }

    #[test]
    fn test_multi_para() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"  <w:p><w:r><w:t>Para1</w:t></w:r></w:p>"#,
            r#"  <w:p><w:r><w:t>Para2</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "Para1");
        assert_eq!(items[1].text, "Para2");
    }

    /// reader 迁移 smoke：现场造一个最小 docx，走完整 ZIP → 事件流路径。
    ///
    /// 纯函数测试覆盖不到 `name().local_name()` 与显式 `escape::unescape`，
    /// 这里用一个真实 zip 包证明事件解析没被改坏（含实体 `&amp;` 解码）。
    #[test]
    fn smoke_parse_minimal_docx_zip() {
        use std::io::Write;

        const DOCUMENT_XML: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:body>"#,
            r#"<w:p><w:r><w:t>Smoke 文本</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>a &amp; b</w:t></w:r></w:p>"#,
            r#"</w:body></w:document>"#
        );

        let dir = std::env::temp_dir().join("docparser_office_docx_smoke");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("min.docx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("word/document.xml", opts).unwrap();
            zw.write_all(DOCUMENT_XML.as_bytes()).unwrap();
            zw.finish().unwrap();
        }

        let raw = DocxRawBackend::default()
            .parse(&path)
            .unwrap();
        // 该文件没有任何结构信号 → 默认 `auto` 会走 raw 快路径；本用例
        // 要验的是事件解析，所以另用显式 `semantic` 对照产项
        let sem = DocxRawBackend::default()
            .with_structure_mode(StructureMode::Semantic)
            .parse(&path)
            .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(raw.pages.len(), 1);
        // raw 仍产项（图片位置要靠 items 承载），组装侧按行产 Paragraph 块、文本走 flat_text
        assert_eq!(raw.pages[0].items.len(), 2);
        assert_eq!(raw.pages[0].flat_text.as_deref(), Some("Smoke 文本\na & b"));
        assert_eq!(sem.pages[0].items.len(), 2);
        assert_eq!(sem.pages[0].items[0].text, "Smoke 文本");
        assert_eq!(sem.pages[0].items[1].text, "a & b");
    }

    /// 极简 docx（纯 `w:p`/`w:t` 段落，零结构信号）端到端：默认 `auto` 判非语义
    /// 走快路径 → 组装层按行产 `Paragraph` 块 → `to_translation_units` 非空。
    ///
    /// 痛点验收：程序生成物 / pandoc 极简输出这类 docx（七信号全无）曾因快路径
    /// 零块被翻译/RAG 消费者报"文档无可翻译文本单元"。
    #[test]
    fn minimal_plain_docx_yields_paragraph_blocks_and_translation_units() {
        use std::io::Write;

        const DOCUMENT_XML: &str = concat!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:p><w:r><w:t>第一段正文</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Second paragraph</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>第三段正文</w:t></w:r></w:p>"#,
            r#"</w:body></w:document>"#
        );

        let dir = std::env::temp_dir().join("docparser_office_docx_minimal_plain");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plain.docx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("word/document.xml", opts).unwrap();
            zw.write_all(DOCUMENT_XML.as_bytes()).unwrap();
            zw.finish().unwrap();
        }

        // 七信号全无 → 默认 auto 走快路径（flat_text 有值、items 不产结构判定）
        let raw = DocxRawBackend::default()
            .parse(&path)
            .unwrap();
        std::fs::remove_file(&path).ok();
        assert!(
            raw.pages[0].flat_text.is_some(),
            "零信号极简 docx 应走快路径"
        );

        use crate::plugin::FormatPlugin;
        let doc = crate::backends::office::OfficePlugin::default().assemble(&raw);
        let page = &doc.pages[0];
        assert_eq!(page.blocks.len(), 3);
        assert!(
            page.blocks
                .iter()
                .all(|b| b.kind == crate::model::BlockKind::Paragraph),
            "快路径只产纯段落块"
        );
        assert_eq!(page.blocks[1].content, "Second paragraph");
        assert_eq!(page.text, "第一段正文\nSecond paragraph\n第三段正文");
        // 痛点验收：极简 docx 必须产翻译单元
        let units = crate::emit::to_translation_units(&doc);
        assert_eq!(units.len(), 3, "极简 docx 必须产翻译单元");
        assert_eq!(units[1].text, "Second paragraph");
    }

    /// 区域部件（页眉/页脚/脚注）：只在 opt-in 时解析，且输出顺序固定为
    /// header → footer → footnote（与 Python `_DOCX_REGION_KINDS` 一致）。
    ///
    /// rels 里故意把 footer 排在 header 前面：顺序必须由 kind 优先级决定，
    /// 不能被 rels 文档序带偏，否则两侧 Note 块次序会对不上。
    #[test]
    fn regions_follow_fixed_kind_order() {
        use std::io::Write;

        const DOCUMENT_XML: &str = concat!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:body><w:p><w:r><w:t>正文</w:t></w:r></w:p></w:body></w:document>"#
        );
        const RELS: &str = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/>"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>"#,
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/>"#,
            r#"</Relationships>"#
        );
        const HEADER: &str = concat!(
            r#"<w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:t>页眉内容</w:t></w:r></w:p></w:hdr>"#
        );
        const FOOTER: &str = concat!(
            r#"<w:ftr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:t>页脚内容</w:t></w:r></w:p></w:ftr>"#
        );
        // 脚注部件里 separator/continuationSeparator 没有文本 → 必须被空段过滤掉
        const FOOTNOTES: &str = concat!(
            r#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:footnote w:type="separator" w:id="0"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>"#,
            r#"<w:footnote w:id="1"><w:p><w:r><w:t>脚注文本</w:t></w:r></w:p></w:footnote>"#,
            r#"</w:footnotes>"#
        );

        let dir = std::env::temp_dir().join("docparser_office_docx_regions");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("regions.docx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("word/document.xml", DOCUMENT_XML),
                ("word/_rels/document.xml.rels", RELS),
                ("word/header1.xml", HEADER),
                ("word/footer1.xml", FOOTER),
                ("word/footnotes.xml", FOOTNOTES),
            ] {
                zw.start_file(name, opts).unwrap();
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }

        let p = &path;
        // 该 fixture 正文无结构信号 → 默认 auto 走 raw（不产项），但区域照旧解析：
        // 正好覆盖「raw + include_regions」这个 Python 也会走到的组合
        let with_regions = DocxRawBackend::new(true).parse(p).unwrap();
        let default = DocxRawBackend::default()
            .with_structure_mode(StructureMode::Semantic)
            .parse(p)
            .unwrap();
        std::fs::remove_file(&path).ok();

        // 默认关：连解析都不做（docs 里也不该出现区域文本）
        assert!(default.pages[0].regions.is_empty());
        assert_eq!(default.pages[0].items.len(), 1);

        let regions = &with_regions.pages[0].regions;
        let got: Vec<(&str, &str)> = regions
            .iter()
            .map(|r| (r.kind.as_str(), r.text.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("header", "页眉内容"),
                ("footer", "页脚内容"),
                ("footnote", "脚注文本")
            ]
        );
        // 区域不进 items（正文口径不受开关影响）；raw 下正文只靠 flat_text 出文本
        assert_eq!(with_regions.pages[0].items.len(), 1);
        assert_eq!(with_regions.pages[0].items[0].text, "正文");
        assert!(with_regions.pages[0].flat_text.is_some());
    }

    /// 批注（`word/comments.xml`）走同一条区域通道：`kind = "comment"`，排在四种既有
    /// 区域之后，`--include-regions` 时由组装层变成 `BlockKind::Note` 块。
    ///
    /// 三个坑一起验：① `commentsExtended.xml` 的 rel Type 也以 `/comments` 开头，
    /// 不能用 `contains` 匹配（否则会多读一个没有 `w:p` 的部件）；② 只有
    /// `w:annotationRef` 的空批注必须被过滤；③ 批注作者/日期不进文本。
    #[test]
    fn comments_become_regions_of_kind_comment() {
        use std::io::Write;

        const DOCUMENT_XML: &str = concat!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:body><w:p><w:r><w:t>正文</w:t></w:r></w:p></w:body></w:document>"#
        );
        const RELS: &str = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>"#,
            r#"<Relationship Id="rId2" Type="http://schemas.microsoft.com/office/2011/relationships/commentsExtended" Target="commentsExtended.xml"/>"#,
            r#"</Relationships>"#
        );
        const COMMENTS: &str = concat!(
            r#"<w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:comment w:id="1" w:author="张三" w:date="2026-10-28T10:00:00Z">"#,
            r#"<w:p><w:r><w:annotationRef/></w:r><w:r><w:t>这里要改</w:t></w:r></w:p></w:comment>"#,
            r#"<w:comment w:id="2" w:author="李四"><w:p><w:r><w:annotationRef/></w:r></w:p></w:comment>"#,
            r#"</w:comments>"#
        );
        // 干扰项：内容模型是 comment id 映射，没有任何 `w:p`
        const COMMENTS_EXTENDED: &str = concat!(
            r#"<w15:commentsEx xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml">"#,
            r#"<w15:commentEx w15:paraId="00000001" w15:done="0"/>"#,
            r#"</w15:commentsEx>"#
        );

        let dir = std::env::temp_dir().join("docparser_office_docx_comments");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("comments.docx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("word/document.xml", DOCUMENT_XML),
                ("word/_rels/document.xml.rels", RELS),
                ("word/comments.xml", COMMENTS),
                ("word/commentsExtended.xml", COMMENTS_EXTENDED),
            ] {
                zw.start_file(name, opts).unwrap();
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }

        let p = &path;
        let with_regions = DocxRawBackend::new(true).parse(p).unwrap();
        let without = DocxRawBackend::default()
            .with_structure_mode(StructureMode::Semantic)
            .parse(p)
            .unwrap();
        std::fs::remove_file(&path).ok();

        // 默认关：不解析批注
        assert!(without.pages[0].regions.is_empty());

        let got: Vec<(&str, &str)> = with_regions.pages[0]
            .regions
            .iter()
            .map(|r| (r.kind.as_str(), r.text.as_str()))
            .collect();
        // 只有一条：空批注被过滤，`commentsExtended` 没被当成批注部件
        assert_eq!(got, [("comment", "这里要改")]);
        // 批注不进正文
        assert_eq!(with_regions.pages[0].items[0].text, "正文");
    }

    /// `raw` 快路径：标题/列表/表格全部压平为行文本（`flat_text`，组装层按行产
    /// 纯 `Paragraph` 块）；`auto` 因 `Heading1` 命中 probe 而仍走 semantic
    /// （对齐 Python `_DOCX_PROBE` 同源判据）。
    #[test]
    fn raw_mode_flattens_to_text_only() {
        use std::io::Write;

        const DOCUMENT_XML: &str = concat!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>第一章</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>要点</w:t></w:r></w:p>"#,
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>甲</w:t></w:r></w:p></w:tc>"#,
            r#"<w:tc><w:p><w:r><w:t>乙</w:t></w:r></w:p></w:tc></w:tr>"#,
            r#"<w:tr><w:tc><w:p><w:r><w:t>丙</w:t></w:r></w:p></w:tc>"#,
            r#"<w:tc><w:p><w:r><w:t>丁</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
            r#"<w:p><w:r><w:t>正文</w:t></w:r></w:p>"#,
            r#"</w:body></w:document>"#
        );

        let dir = std::env::temp_dir().join("docparser_office_docx_raw");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("raw.docx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("word/document.xml", opts).unwrap();
            zw.write_all(DOCUMENT_XML.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let p = path.clone();

        let raw = DocxRawBackend::default()
            .with_structure_mode(StructureMode::Raw)
            .parse(&p)
            .unwrap();
        let auto = DocxRawBackend::default().parse(&p).unwrap();
        std::fs::remove_file(&path).ok();

        // raw：页文本按 "\n" 拼接，表格行内 "," 行间 "\n"
        // （items 仍在；`flat_text` 有值时 assemble 按行产纯 Paragraph 块）
        let page = &raw.pages[0];
        assert_eq!(page.items.len(), 4);
        assert_eq!(
            page.flat_text.as_deref(),
            Some("第一章\n要点\n甲,乙\n丙,丁\n正文")
        );
        // auto：文档里有 Heading1 → probe 命中 → 仍是全结构
        assert!(
            auto.pages[0].flat_text.is_none(),
            "auto 命中结构信号走 semantic"
        );
        assert_eq!(auto.pages[0].items.len(), 4);
        assert_eq!(auto.pages[0].items[0].level, Some(1));
        assert_eq!(auto.pages[0].items[1].list, Some(false));
        assert!(auto.pages[0].items[2].table.is_some());
    }

    /// 文档序的图片 rid：段落里的 blip 与表格里的 blip 都按出现顺序收集，
    /// 且同一元素内的重复引用只算一次（对齐 Python `_blip_rids` 的 `v not in rids`）。
    #[test]
    fn image_rids_keep_document_order() {
        let xml = concat!(
            r#"<w:body>"#,
            r#"<w:p><w:r><w:t>前</w:t></w:r>"#,
            r#"<w:r><w:drawing><a:blip r:embed="rId1"/><a:blip r:embed="rId1"/></w:drawing></w:r></w:p>"#,
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>格</w:t></w:r>"#,
            r#"<w:r><w:drawing><a:blip r:embed="rId2"/></w:drawing></w:r></w:p></w:tc></w:tr></w:tbl>"#,
            r#"<w:p><w:r><w:drawing><v:imagedata r:id="rId3"/></w:drawing></w:r></w:p>"#,
            r#"</w:body>"#
        );
        assert_eq!(parse_rids(xml), vec!["rId1", "rId2", "rId3"]);
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0, 0, 0, 13]);
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    /// 一段带内嵌图的段落（`r:embed` 指向 rels 里的图片关系）。
    fn para_with_image(text: &str, rid: &str) -> String {
        format!(
            concat!(
                r#"<w:p><w:r><w:t>{text}</w:t></w:r>"#,
                r#"<w:r><w:drawing><wp:inline><a:graphic><a:graphicData>"#,
                r#"<pic:pic><pic:blipFill><a:blip r:embed="{rid}"/></pic:blipFill></pic:pic>"#,
                r#"</a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"#
            ),
            text = text,
            rid = rid
        )
    }

    /// 内嵌图（`ocr_images`）：
    /// * 图片项落在**同段落文本项之后**（Python `items.append(("image", im))`）；
    /// * 整篇一个去重域——同一 media 第二次引用不再产项；
    /// * 装饰图（`min(w,h) < 50`）不进项，但**已占去重名额**；
    /// * 默认关（不读字节）；raw 模式即便开了也不出图（Python 快路径直接 return），
    ///   只有 `raw + include_regions`（落回慢路径）才收集。
    #[test]
    fn images_respect_position_dedup_and_filters() {
        use std::io::Write;

        const RELS: &str = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/pic1.png"/>"#,
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/tiny.png"/>"#,
            r#"<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/pic2.png"/>"#,
            r#"</Relationships>"#
        );
        let document = format!(
            concat!(
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
                r#"{}"#, // 图前 + pic1
                r#"{}"#, // 图后 + pic1（重复 → 去重）
                r#"{}"#, // tiny（20×20 → 过滤）
                r#"{}"#, // pic2
                r#"</w:body></w:document>"#
            ),
            para_with_image("图前", "rId1"),
            para_with_image("图后", "rId1"),
            para_with_image("", "rId2"),
            para_with_image("", "rId3"),
        );

        let dir = std::env::temp_dir().join("docparser_office_docx_images");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("images.docx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("word/document.xml", opts).unwrap();
            zw.write_all(document.as_bytes()).unwrap();
            zw.start_file("word/_rels/document.xml.rels", opts).unwrap();
            zw.write_all(RELS.as_bytes()).unwrap();
            zw.start_file("word/media/pic1.png", opts).unwrap();
            zw.write_all(&png_bytes(120, 80)).unwrap();
            zw.start_file("word/media/tiny.png", opts).unwrap();
            zw.write_all(&png_bytes(20, 20)).unwrap();
            zw.start_file("word/media/pic2.png", opts).unwrap();
            zw.write_all(&png_bytes(64, 64)).unwrap();
            zw.finish().unwrap();
        }
        let p = path.clone();

        let on = DocxRawBackend::default()
            .with_ocr_images(true)
            .parse(&p)
            .unwrap();
        let off = DocxRawBackend::default().parse(&p).unwrap();
        let raw = DocxRawBackend::default()
            .with_structure_mode(StructureMode::Raw)
            .with_ocr_images(true)
            .parse(&p)
            .unwrap();
        let raw_regions = DocxRawBackend::new(true)
            .with_structure_mode(StructureMode::Raw)
            .with_ocr_images(true)
            .parse(&p)
            .unwrap();
        std::fs::remove_file(&path).ok();

        let items = &on.pages[0].items;
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| if i.image.is_some() { "image" } else { "text" })
            .collect();
        assert_eq!(kinds, ["text", "image", "text", "image"]);
        assert_eq!(items[0].text, "图前");
        assert_eq!(items[2].text, "图后");
        let first = items[1].image.as_ref().expect("第一张图");
        assert_eq!(first.ext, "png");
        assert_eq!(first.px, Some((120, 80)));
        assert_eq!(first.data, png_bytes(120, 80));
        assert_eq!(items[3].image.as_ref().unwrap().px, Some((64, 64)));

        // 组装层：Page.images 两张 + 两个空 content 的 Image 块，图片不进页文本
        let doc = crate::backends::office::assemble(&on, &crate::backends::office::OfficeOptions::default());
        let page = &doc.pages[0];
        assert_eq!(page.images.len(), 2);
        assert_eq!(
            page.images[0].dedup_key.as_deref(),
            Some(crate::backends::office::image::dedup_key(&png_bytes(120, 80)).as_str())
        );
        assert!(page.images[0].data.is_empty(), "字节不落 IR");
        let block_kinds: Vec<_> = page.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            block_kinds,
            [
                crate::model::BlockKind::Paragraph,
                crate::model::BlockKind::Image,
                crate::model::BlockKind::Paragraph,
                crate::model::BlockKind::Image,
            ]
        );
        assert_eq!(page.blocks[1].content, "");
        assert_eq!(page.text, "图前\n\n图后");

        // 默认关：只有文本项
        assert_eq!(off.pages[0].items.len(), 2);
        assert!(off.pages[0].items.iter().all(|i| i.image.is_none()));

        // raw：Python 快路径不收集图片；开了 include_regions 才落回慢路径
        assert!(raw.pages[0].images.is_empty());
        assert!(raw.pages[0].items.iter().all(|i| i.image.is_none()));
        assert_eq!(raw.pages[0].flat_text.as_deref(), Some("图前\n图后"));
        assert_eq!(
            raw_regions
                .pages
                .iter()
                .flat_map(|pg| pg.items.iter())
                .filter(|i| i.image.is_some())
                .count(),
            2
        );
    }

    /// 信物发号形态（影子割接的解析侧验收）：普通段 / 标题 / 空段丢弃 /
    /// 表格格合并（slot_count>1）/ sdt 续号，逐项断言 token 形态与槽位表
    /// 内容——发号规则 = item 发射序（见 [`DocxSlotSidecar`] 文档）。
    #[test]
    fn source_token_issuance_shapes() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            // 槽0：普通段
            r#"<w:p><w:r><w:t>普通段落。</w:t></w:r></w:p>"#,
            // 槽1：标题段（pPr span 记录）
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>标题。</w:t></w:r></w:p>"#,
            // 空段：不进 items、不占号
            r#"<w:p><w:pPr/></w:p>"#,
            // 槽2..4：表格两项候选格（第二行首格空 → 丢弃；共 2 格 slot_count=2）
            r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>甲格</w:t></w:r></w:p></w:tc></w:tr>"#,
            r#"<w:tr><w:tc><w:p><w:r><w:t xml:space="preserve"> </w:t></w:r></w:p></w:tc>"#,
            r#"<w:tc><w:p><w:r><w:t>乙格</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
            // 槽4：sdt 内段落续号
            r#"<w:sdt><w:sdtContent><w:p><w:r><w:t>控件段落。</w:t></w:r></w:p></w:sdtContent></w:sdt>"#,
            r#"</w:body>"#
        );
        let (nodes, sidecar) = docx_raw_text_with_slots(xml, &Numbering::default(), &HashMap::new())
            .unwrap();

        // item 序 = [普通段, 标题, 表格, sdt 段]；信物按序发号
        let tokens: Vec<_> = nodes
            .iter()
            .filter_map(|n| match n {
                DocxNode::Item(it) => Some(it.source_token),
                _ => None,
            })
            .collect();
        assert_eq!(
            tokens,
            vec![
                Some(SourceToken { first_slot: 0, slot_count: 1 }),
                Some(SourceToken { first_slot: 1, slot_count: 1 }),
                Some(SourceToken { first_slot: 2, slot_count: 2 }), // 两候选格合并
                Some(SourceToken { first_slot: 4, slot_count: 1 }),
            ]
        );

        // 槽位表内容：段落文本未 trim、格槽位 trim 非空、空段/空格不占号
        assert_eq!(sidecar.slots.len(), 5);
        match &sidecar.slots[0] {
            DocxSlot::Para(p) => {
                assert_eq!(p.text, "普通段落。");
                assert!(p.ppr.is_none());
                assert_eq!(p.text_spans.len(), 1);
                assert!(xml[p.start..p.end].contains("普通段落。"));
            }
            _ => panic!("槽0 应为段落位"),
        }
        match &sidecar.slots[1] {
            DocxSlot::Para(p) => {
                let (s, e) = p.ppr.expect("标题段应记录 pPr span");
                assert_eq!(&xml[s..e], r#"<w:pPr><w:pStyle w:val="Heading1"/></w:pPr>"#);
            }
            _ => panic!("槽1 应为段落位"),
        }
        match &sidecar.slots[2] {
            DocxSlot::Cell(c) => {
                assert_eq!(c.text, "甲格");
                assert_eq!(c.paras.len(), 1);
                assert!(!c.has_nested_table);
                assert!(xml[c.start..c.end].contains("甲格"));
            }
            _ => panic!("槽2 应为格位"),
        }
        match &sidecar.slots[3] {
            DocxSlot::Cell(c) => assert_eq!(c.text, "乙格"),
            _ => panic!("槽3 应为格位"),
        }
        match &sidecar.slots[4] {
            DocxSlot::Para(p) => assert_eq!(p.text, "控件段落。"),
            _ => panic!("槽4 应为段落位"),
        }

        // 图片项不发号：表格后跟 ImageRids 节点不影响上述计数（上面 token 序
        // 已隐式验证——表格项之后的 sdt 段仍从 4 续号）
    }

    // -------------------------------------------------------------------------
    // 缺陷①/②采集测试：Fallback 子树跳过 + 主导 rPr 选举 + 槽位超集字段
    // -------------------------------------------------------------------------

    /// 单段文档 → 该段的段落槽位（缺陷①/②采集断言的便捷入口）。
    fn solo_para_slot(xml: &str) -> DocxParaSlot {
        let (_, sidecar) =
            docx_raw_text_with_slots(xml, &Numbering::default(), &HashMap::new()).unwrap();
        match sidecar.slots.into_iter().next() {
            Some(DocxSlot::Para(p)) => p,
            other => panic!("应有且仅有一个段落槽位，得到 {other:?}"),
        }
    }

    /// 缺陷①：`mc:Fallback` 子树整树跳过——Choice 分支文本照常提取，Fallback
    /// 的同内容 VML 镜像不进文本、不进信物 span（文本与发号同步跳过）。
    #[test]
    fn fallback_subtree_skipped_from_text_and_spans() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r>"#,
            r#"<mc:AlternateContent><mc:Choice Requires="wps">"#,
            r#"<w:drawing><w:txbxContent><w:p><w:r><w:t>Choice 文本甲。</w:t></w:r></w:p></w:txbxContent></w:drawing>"#,
            r#"</mc:Choice>"#,
            r#"<mc:Fallback>"#,
            r#"<w:pict><v:shape><v:textbox><w:txbxContent><w:p><w:r><w:t>Fallback 镜像乙。</w:t></w:r></w:p></w:txbxContent></v:textbox></v:shape></w:pict>"#,
            r#"</mc:Fallback></mc:AlternateContent>"#,
            r#"</w:r><w:r><w:t>段落尾。</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items.len(), 1, "Fallback 镜像不得成段");
        assert_eq!(items[0].text, "Choice 文本甲。段落尾。");
        assert!(!items[0].text.contains("镜像"), "镜像文本不得进单元流");

        let slot = solo_para_slot(xml);
        // 文本与信物 span 同步跳过：text_spans 只含 Choice 的 w:t 与段落尾 w:t
        assert_eq!(slot.text_spans.len(), 2);
        assert_eq!(slot.text, "Choice 文本甲。段落尾。");
        // Choice 分支带 w:drawing → 回写保真路径判据为 true（Fallback 里的
        // w:pict 已随子树跳过，不影响判据——drawing 已足以触发）
        assert!(slot.has_non_text_inline);
    }

    /// 缺陷①：自闭合 `<mc:Fallback/>` 是 Empty 事件，不触发整树跳过（免疫）。
    #[test]
    fn fallback_self_closing_is_ignored() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:t>前后文本。</w:t></w:r><mc:Fallback/></w:p>"#,
            r#"</w:body>"#
        );
        let items = parse_items(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "前后文本。");
    }

    /// 缺陷②：主导 rPr 选举——可见字符占比最高的 run 胜出。
    #[test]
    fn dominant_rpr_elects_longest_run() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:t>短</w:t></w:r>"#,
            r#"<w:r><w:rPr><w:b/></w:rPr><w:t>这里是最长的加粗正文内容。</w:t></w:r>"#,
            r#"<w:r><w:rPr><w:i/></w:rPr><w:t>斜体次长。</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let slot = solo_para_slot(xml);
        let (s, e) = slot.dominant_rpr_span.expect("最长 run 带 rPr 应当选");
        assert_eq!(&xml[s..e], "<w:rPr><w:b/></w:rPr>");
    }

    /// 缺陷②：上下标 run（`w:vertAlign`）不参选——即便它字符最多（劫持场景：
    /// 段首上标角标 ¹ 不得把 vertAlign 继承给整段译文）。
    #[test]
    fn dominant_rpr_excludes_vert_align_runs() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:rPr><w:vertAlign w:val="superscript"/></w:rPr><w:t>1234567890</w:t></w:r>"#,
            r#"<w:r><w:rPr><w:b/></w:rPr><w:t>正文。</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let slot = solo_para_slot(xml);
        let (s, e) = slot.dominant_rpr_span.expect("非上标 run 应当选");
        assert_eq!(&xml[s..e], "<w:rPr><w:b/></w:rPr>", "上标 run 不得当选");
    }

    /// 缺陷②：`w:pPr` 内的 rPr（段落标记 rPr）不参选——只认 run 直属
    /// （结构性保证：pPr 子树由 para_props 整树消费，不进段落事件循环）。
    #[test]
    fn dominant_rpr_ignores_ppr_internal_rpr() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:pPr><w:rPr><w:b/><w:sz w:val="40"/></w:rPr></w:pPr>"#,
            r#"<w:r><w:rPr><w:i/></w:rPr><w:t>正文文字若干。</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let slot = solo_para_slot(xml);
        let (s, e) = slot.dominant_rpr_span.expect("run 直属 rPr 应当选");
        assert_eq!(&xml[s..e], "<w:rPr><w:i/></w:rPr>", "pPr 内 rPr 不入选举");
    }

    /// 缺陷②：无 rPr 的 run 字符最多时 dominant = None（口径：缺省样式即主导
    /// 样式）→ 快路径译文 run 不带 rPr 切片。
    #[test]
    fn dominant_rpr_none_when_default_run_wins() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>短</w:t></w:r>"#,
            r#"<w:r><w:t>不带样式的正文明显更长，应当胜出。</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let slot = solo_para_slot(xml);
        assert!(
            slot.dominant_rpr_span.is_none(),
            "无 rPr run 胜出 → dominant = None（若误选带 rPr 短 run 会得 Some）"
        );
    }

    /// 缺陷②：字符数平手取文档序先者（确定性口径）。
    #[test]
    fn dominant_rpr_tie_takes_first() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>甲乙丙丁</w:t></w:r>"#,
            r#"<w:r><w:rPr><w:i/></w:rPr><w:t>戊己庚辛</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let slot = solo_para_slot(xml);
        let (s, e) = slot.dominant_rpr_span.expect("平手仍应有一者当选");
        assert_eq!(&xml[s..e], "<w:rPr><w:b/></w:rPr>", "平手取先者");
    }

    /// 缺陷②采集：has_non_text_inline 判据——drawing / fldSimple 命中；布局
    /// 承载元素（tab、软 br、分页 br、noBreakHyphen）全部命中（提取里是
    /// \t/\n 占位、渲染里是布局——快路径整段替换会删掉：软换行变单行、
    /// 丢 tab/丢分页）；纯文本不命中（唯一快路径形态）。
    #[test]
    fn has_non_text_inline_detection() {
        let wrap = |inner: &str| {
            format!(
                r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p>{inner}</w:p></w:body>"#
            )
        };
        for (inner, why) in [
            (
                r#"<w:r><w:t>甲</w:t></w:r><w:r><w:drawing><a:blip r:embed="rId1"/></w:drawing></w:r>"#,
                "drawing",
            ),
            (
                r#"<w:fldSimple w:instr=" X "><w:r><w:t>甲乙丙。</w:t></w:r></w:fldSimple>"#,
                "fldSimple",
            ),
            (
                r#"<w:r><w:br w:type="page"/></w:r><w:r><w:t>甲乙丙。</w:t></w:r>"#,
                "分页 br",
            ),
        ] {
            let slot = solo_para_slot(&wrap(inner));
            assert!(slot.has_non_text_inline, "{why} 应判为内联物件");
        }
        for (inner, why) in [
            (r#"<w:r><w:t>甲乙丙丁。</w:t></w:r>"#, "纯文本"),
        ] {
            let slot = solo_para_slot(&wrap(inner));
            assert!(!slot.has_non_text_inline, "{why} 不应命中（快路径）");
        }
        // 布局承载元素（主管修正）：提取里是 \t/\n 占位、渲染里是布局——
        // 与内联物件同判，快路径整段净荷替换会删掉它们
        for (inner, why) in [
            (r#"<w:r><w:t>甲乙。</w:t><w:br/><w:t>丙丁。</w:t></w:r>"#, "软 br"),
            (
                r#"<w:r><w:t>甲乙。</w:t></w:r><w:tab/><w:r><w:t>丙丁。</w:t></w:r>"#,
                "tab",
            ),
        ] {
            let slot = solo_para_slot(&wrap(inner));
            assert!(slot.has_non_text_inline, "{why} 应判为布局承载");
        }
    }

    /// 缺陷②采集：wt_spans = `w:t` 完整元素区间；inner_content = pPr 之后到
    /// `</w:p>` 起符（无 pPr = 开始标签之后）——快路径整段替换区间。
    #[test]
    fn wt_spans_and_inner_content_capture() {
        let xml = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:t>甲</w:t></w:r><w:r><w:t>乙</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let slot = solo_para_slot(xml);
        assert_eq!(slot.wt_spans.len(), 2);
        let (s, e) = slot.wt_spans[0];
        assert_eq!(&xml[s..e], "<w:t>甲</w:t>", "wt span = 完整 w:t 元素");
        let (s, e) = slot.wt_spans[1];
        assert_eq!(&xml[s..e], "<w:t>乙</w:t>");
        let (s, e) = slot.inner_content;
        assert_eq!(
            &xml[s..e],
            r#"<w:r><w:t>甲</w:t></w:r><w:r><w:t>乙</w:t></w:r>"#,
            "净荷 = pPr 之后到 </w:p> 起符"
        );

        // 无 pPr：净荷起点 = `<w:p>` 开始标签之后
        let xml2 = concat!(
            r#"<w:body xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:t>全净荷。</w:t></w:r></w:p>"#,
            r#"</w:body>"#
        );
        let slot = solo_para_slot(xml2);
        assert!(slot.ppr.is_none());
        let (s, e) = slot.inner_content;
        assert_eq!(&xml2[s..e], r#"<w:r><w:t>全净荷。</w:t></w:r>"#);
    }
}

// ---------------------------------------------------------------------------
// docforge 补丁测试：styles.xml 中文数字 styleId → 标题层级
// ---------------------------------------------------------------------------

#[cfg(test)]
mod styles_map_tests {
    use super::*;

    #[test]
    fn styles_map_from_names() {
        let xml = r#"<?xml version="1.0"?>
<w:styles xmlns:w="urn:x">
  <w:style w:type="paragraph" w:styleId="1"><w:name w:val="heading 1"/><w:basedOn w:val="a"/></w:style>
  <w:style w:type="paragraph" w:styleId="2"><w:name w:val="heading 2"/></w:style>
  <w:style w:type="paragraph" w:styleId="a"><w:name w:val="Normal"/></w:style>
  <w:style w:type="paragraph" w:styleId="4"><w:name w:val="标题 4"/></w:style>
</w:styles>"#;
        let map = parse_styles_heading_map(xml);
        assert_eq!(map.get("1"), Some(&1), "zh-CN 数字 styleId 经 name 判定");
        assert_eq!(map.get("2"), Some(&2));
        assert_eq!(map.get("4"), Some(&4), "中文样式名同样入表");
        assert!(!map.contains_key("a"), "Normal 不入表");
    }

    #[test]
    fn numeric_pstyle_resolved_via_styles_map() {
        // document.xml 用数字 styleId（zh-CN Word 风格），样式名在 styles.xml
        let styles = parse_styles_heading_map(
            r#"<w:styles xmlns:w="urn:x"><w:style w:styleId="2"><w:name w:val="heading 2"/></w:style></w:styles>"#,
        );
        let doc = r#"<w:document xmlns:w="urn:x"><w:body>
<w:p><w:pPr><w:pStyle w:val="2"/></w:pPr><w:r><w:t>数字样式标题</w:t></w:r></w:p>
<w:p><w:r><w:t>正文段落。</w:t></w:r></w:p>
</w:body></w:document>"#;
        let nodes = docx_raw_text(doc, &Numbering::default(), &styles).unwrap();
        let items: Vec<_> = nodes
            .into_iter()
            .filter_map(|n| match n {
                DocxNode::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items[0].level, Some(2), "数字 styleId 经 styles 映射出层级");
        assert_eq!(items[1].level, None);
    }

    #[test]
    fn styles_map_corrupt_xml_yields_empty() {
        let map = parse_styles_heading_map("<w:styles 未闭合");
        assert!(map.is_empty());
    }
}
