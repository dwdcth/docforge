// 吸收自 docparser-office 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：M2b 仅吸收 DOCX 相关（xlsx/pptx/merge 留 M3）；crate 引用重写。
//! docparser Office 后端：DOCX / XLSX / PPTX 解析（`zip` + `quick-xml` 流式）。
//!
//! 与 Python `docparser/plugins/ooxml.py` 语义对齐：相同 IR/错误码/空白口径，
//! 但使用 `quick-xml` 流式解析（移除 `expat_serialized` 全局锁）。
//! 注意：解析是单线程流式的，未做 sheet/slide 级并行（原 `rayon` 依赖实际上未使用，已移除）。
//!
//! 架构（继承 PDF 后端的注册模式）：
//!   * `docx::DocxRawBackend` / `xlsx::XlsxRawBackend` / `pptx::PptxRawBackend` →
//!     `ParseCapability`（流式提取文本 + 结构：标题层级 / 列表 / 表格 / 图表）
//!   * `OfficePlugin` → `FormatPlugin`（RawDoc → IR Document 组装）
//!   * `register_defaults()` → 供 `core-python` 和 `cli` 调用

use std::path::Path;
use std::sync::Arc;

// `DocError`/`Result` 只被 docx/xlsx 共用的 zip 读取辅助用到；只剩 pptx 时会成为未用导入。
#[cfg(any(feature = "docx", feature = "xlsx"))]
use crate::error::{DocError, Result};
use crate::plugin::{FormatPlugin, RawDoc, RawPage};
use crate::registry::Registry;

#[cfg(feature = "docx")]
pub mod docx;
#[cfg(feature = "pptx")]
pub mod pptx;
#[cfg(feature = "xlsx")]
pub mod xlsx;
pub(crate) mod image;
pub(crate) mod parallel;

pub mod structure;


pub mod merge;
pub use merge::MergeCellPolicy;
pub use structure::StructureMode;

/// 本 crate 实际编译进来的格式列表（随 feature 变化）。
pub const FORMATS: &[&str] = &[
    #[cfg(feature = "docx")]
    "docx",
    #[cfg(feature = "xlsx")]
    "xlsx",
    #[cfg(feature = "pptx")]
    "pptx",
];

/// OOXML 通用 namespace URI（quick-xml 0.42 起统一为 `&str`）。
pub mod ns {
    pub const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    pub const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    pub const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
    pub const C: &str = "http://schemas.openxmlformats.org/drawingml/2006/chart";
    pub const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    pub const SHEET: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    pub const REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    pub const VML: &str = "urn:schemas-microsoft-com:vml";
    pub const WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
}

/// OOXML 元素**本地名**（去命名空间前缀）。
///
/// quick-xml 0.42 的 `Name::local_name()` 不返回 `w:`/`a:`/`p:` 前缀，因此这里只存本地名，
/// 比较一律用 `e.name().local_name().as_ref()`。副作用：`w:t` 与 `a:t` 都是 `"t"`，
/// 同名不同前缀的元素在本地名比较下不可区分——Office 的正文里不存在这种歧义元素。
pub mod tags {
    // docx
    pub const W_P: &str = "p";
    pub const W_TBL: &str = "tbl";
    pub const W_TR: &str = "tr";
    pub const W_TC: &str = "tc";
    pub const W_T: &str = "t";
    pub const W_R: &str = "r";
    pub const W_TAB: &str = "tab";
    pub const W_BR: &str = "br";
    pub const W_CR: &str = "cr";
    pub const W_SDT: &str = "sdt";
    pub const W_SDTC: &str = "sdtContent";
    pub const W_PPR: &str = "pPr";
    pub const W_PSTYLE: &str = "pStyle";
    pub const W_OUTLINE_LVL: &str = "outlineLvl";
    /// 不间断连字符（渲染为 `-`）
    pub const W_NBH: &str = "noBreakHyphen";
    pub const W_NUMPR: &str = "numPr";
    pub const W_NUMID: &str = "numId";
    pub const W_ILVL: &str = "ilvl";
    // docx numbering.xml
    pub const W_ABSTRACTNUM: &str = "abstractNum";
    pub const W_ABSTRACTNUMID: &str = "abstractNumId";
    pub const W_NUM: &str = "num";
    pub const W_LVL: &str = "lvl";
    pub const W_NUMFMT: &str = "numFmt";
    pub const VAL: &str = "val";
    pub const W_BODY: &str = "body";
    pub const W_TXBX: &str = "txbxContent";
    // xlsx
    pub const SHEET_DATA: &str = "sheetData";
    pub const ROW: &str = "row";
    pub const C: &str = "c";
    pub const V: &str = "v";
    pub const IS: &str = "is";
    pub const T: &str = "t";
    pub const SI: &str = "si";
    pub const SST: &str = "sst";
    pub const NUM_FMT: &str = "numFmt";
    pub const CELL_XFS: &str = "cellXfs";
    pub const XF: &str = "xf";
    // pptx
    pub const P_SP: &str = "sp";
    pub const P_PIC: &str = "pic";
    pub const P_GRPSP: &str = "grpSp";
    pub const P_GFRAME: &str = "graphicFrame";
    pub const P_CXNSP: &str = "cxnSp";
    pub const P_SPTREE: &str = "spTree";
    pub const P_TXBODY: &str = "txBody";
    pub const P_PH: &str = "ph";
    pub const P_SLD: &str = "sld";
    pub const P_SLDID: &str = "sldId";
    pub const A_T: &str = "t";
    pub const A_BR: &str = "br";
    pub const A_TBL: &str = "tbl";
    pub const A_TR: &str = "tr";
    pub const A_TC: &str = "tc";
    pub const A_BLIP: &str = "blip";
    /// VML 时代的图片引用节点（`v:imagedata r:id="rId..."`，老 docx 仍有）
    pub const VML_IMAGEDATA: &str = "imagedata";
    pub const A_OFF: &str = "off";
    pub const A_POS: &str = "pos";
    pub const A_BUCHAR: &str = "buChar";
    pub const A_BUAUTONUM: &str = "buAutoNum";
    // chart
    pub const C_CHART: &str = "chart";
    pub const C_TITLE: &str = "title";
    pub const C_DLBL: &str = "dLbl";
    pub const C_TX: &str = "tx";
    pub const C_V: &str = "v";
}

/// Office 后端的组装选项（对齐 Python `BackendPolicy` 里 office 相关的键）。
///
/// 沿用 PDF 的 `register_with` 模式：选项在**注册时注入**，不改 `ParseCapability` / `FormatPlugin`
/// 的公共签名。默认值 = Python 默认 policy，也就是「不传选项时行为与历史完全一致」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OfficeOptions {
    /// 备注/页眉/页脚/脚注等区域内容是否作为独立 `BlockKind::Note` 块产出
    /// （对应 Python `policy.include_regions`，默认 `false`：只并入页文本）。
    pub include_regions: bool,
    /// 截断阈值（字节）：文件大小**超过**它才启用 `truncate_size` 预算
    /// （对应 Python `policy.truncate_threshold`；`0` = 关闭）。
    pub truncate_threshold: u64,
    /// 截断预算（字符数）：xlsx 按行累计、pptx 按页累计，超出即标 `truncated`
    /// （对应 Python `policy.truncate_size`；`0` = 关闭）。
    pub truncate_size: usize,
    /// sheet 上限（`0` = 不限）：超出直接丢弃，**不**置 `truncated`
    /// （对应 Python `policy.max_sheets`：列表切片后丢弃）。
    pub max_sheets: usize,
    /// 幻灯片上限（`0` = 不限）：语义同 `max_sheets`
    /// （对应 Python `policy.max_slides`）。
    pub max_slides: usize,
    /// 结构还原三态（对应 Python `policy.structure_mode`，默认 `auto`）。
    /// `raw` / `auto` 判定为 raw 时：docx/pptx 走行文本快路径（`flat_text`），
    /// 不做结构还原（无标题/列表/表格块），组装层按行产 `Paragraph` 块；
    /// pptx 备注丢弃；xlsx 不受影响（Python 侧同样不看）。
    pub structure_mode: StructureMode,
    /// 是否提取内嵌位图（对应 Python `policy.ocr_images`，默认 `false`）：
    /// 命中白名单且 `min(w,h) >= 50` 的图进 `Page.images`（`dedup_key` = md5），
    /// docx/pptx 语义模式下**另有**一个空 content 的 `BlockKind::Image` 占位
    /// （OCR 文本由 Python 侧管线填，Rust 只提取不算）。
    pub ocr_images: bool,
    /// xlsx 合并单元格处置（对应 Python `policy.merge_cell_policy`，默认 `mark`）：
    /// `mark` 只标注 `meta["merges"]`、`expand` 把左上值铺满区域、`ignore` 不标注。
    pub merge_cell_policy: MergeCellPolicy,
}

impl OfficeOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// 打开/关闭区域块产出（`true` = 与 Python `include_regions=True` 同口径）。
    pub fn with_include_regions(mut self, yes: bool) -> Self {
        self.include_regions = yes;
        self
    }

    /// 设置截断预算（`0` = 关闭；两个值都 > 0 才生效，语义同 Python `_budget`）。
    pub fn with_truncate(mut self, threshold: u64, size: usize) -> Self {
        self.truncate_threshold = threshold;
        self.truncate_size = size;
        self
    }

    /// 设置 sheet 上限（`0` = 不限；语义同 Python `policy.max_sheets`）。
    pub fn with_max_sheets(mut self, max: usize) -> Self {
        self.max_sheets = max;
        self
    }

    /// 设置幻灯片上限（`0` = 不限；语义同 Python `policy.max_slides`）。
    pub fn with_max_slides(mut self, max: usize) -> Self {
        self.max_slides = max;
        self
    }

    /// 设置结构还原三态（语义同 Python `policy.structure_mode`，默认 `auto`）。
    pub fn with_structure_mode(mut self, mode: StructureMode) -> Self {
        self.structure_mode = mode;
        self
    }

    /// 打开/关闭内嵌位图提取（语义同 Python `policy.ocr_images`，默认关）。
    pub fn with_ocr_images(mut self, yes: bool) -> Self {
        self.ocr_images = yes;
        self
    }

    /// 设置合并单元格处置（语义同 Python `policy.merge_cell_policy`，默认 `mark`）。
    pub fn with_merge_cell_policy(mut self, policy: MergeCellPolicy) -> Self {
        self.merge_cell_policy = policy;
        self
    }
}

/// Office 插件：RawDoc → IR Document 组装（与 Python `plugins/ooxml.py` 的 FormatPlugin 语义对齐）。
#[derive(Default)]
pub struct OfficePlugin {
    options: OfficeOptions,
}

impl FormatPlugin for OfficePlugin {
    fn id(&self) -> &str {
        "office"
    }
    fn formats(&self) -> &[&str] {
        FORMATS
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["office-raw"] // 当前只有 raw 后端；后续可增加 semantic 后端
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        // opt-in 链：能力未注册时 resolve 静默跳过（registry filter_map 语义）
        #[allow(unused_mut)]
        let mut chain: Vec<&'static str> = vec!["ocr-images"];
        #[cfg(feature = "lang-detect")]
        chain.push("lang-detect");
        chain
    }
    fn assemble(&self, raw: &RawDoc) -> crate::model::Document {
        assemble(raw, &self.options)
    }
}

/// `RawDoc` → `Document`（`OfficePlugin` 的实际实现；选项通过注册时注入，见 [`OfficeOptions`]）。
fn assemble(raw: &RawDoc, options: &OfficeOptions) -> crate::model::Document {
    // 段落逐条还原：带 table 的条目（docx `w:tbl` / xlsx sheet）还原成 Table，
    // 带 level 的条目（docx 的 pStyle=HeadingN / outlineLvl）还原成 Heading，
    // 带 list 的条目（docx 的 w:numPr）还原成 List，其余一律 Paragraph
    // （不做缩进/编号文本等更深结构还原）。表格 > 标题 > 列表。
    let pages: Vec<crate::model::Page> = raw
        .pages
        .iter()
        .map(|rp| {
            let mut blocks: Vec<crate::model::Block> = Vec::new();
            // 内嵌位图（`ocr_images`）：docx/pptx 走 item（带文档位置），xlsx 只有
            // `rp.images`（drawing 里没有正文位置）——两者合并成 `Page.images`
            let mut images: Vec<crate::model::Image> =
                rp.images.iter().map(to_ir_image).collect();
            for it in &rp.items {
                if let Some(im) = it.image.as_ref() {
                    images.push(to_ir_image(im));
                }
            }
            // 快路径（`structure_mode` 非语义，docx/pptx 解析阶段置 `flat_text`）：
            // 页文本直接用解析阶段拼好的行文本；块层面**按行产 `Paragraph` 块**
            // （trim 后非空的行才产，空行跳过，与 txt 插件"空行不作为块"同口径）。
            //
            // 刻意偏离 Python 非语义分支 `Page(blocks=_region_blocks(...))` 的零块形态：
            // 零块文档对翻译/RAG 消费者（`emit::to_translation_units` 等）等于不可用
            // ——纯段落极简 docx/pptx 会直接报"无可翻译文本单元"。Python 对齐性
            // 在此让位于消费者可用性，`flat_text` 的语义相应降级为"无**结构**块"
            // （页文本层不受影响，仍取 flat 原串，零文本面回归）。
            // 区域/批注仍按 `include_regions` 产 Note 块（排在正文行块之后），
            // 备注（`notes`）整体忽略（Python 非语义分支同样不看 notes）。
            if let Some(flat) = rp.flat_text.as_deref() {
                for line in flat.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    blocks.push(crate::model::Block {
                        kind: crate::model::BlockKind::Paragraph,
                        content: trimmed.to_string(),
                        ..Default::default()
                    });
                }
                if options.include_regions {
                    push_region_notes(&mut blocks, rp);
                }
                return crate::model::Page {
                    name: rp.name.clone(),
                    blocks,
                    text: flat.to_string(),
                    images,
                    ..Default::default()
                };
            }
            let mut parts: Vec<String> = Vec::new();
            for it in &rp.items {
                // 图片项：只产 Image 块（content = ocr_text 或空串），不进页文本
                // （对齐 Python `Block(kind=IMAGE, content=im.meta.get("ocr_text") or "")`）
                if let Some(im) = it.image.as_ref() {
                    blocks.push(crate::model::Block {
                        kind: crate::model::BlockKind::Image,
                        content: im.ocr_text.clone().unwrap_or_default(),
                        // 图片项不发号（恒 None），透传保持口径统一
                        source_token: it.source_token,
                        ..Default::default()
                    });
                    continue;
                }
                if let Some(t) = &it.table {
                    // 表格块 content 取表头（首行），行列结构走 TableData；
                    // 页文本层用展开后的全部行（对齐 Python `blocks_to_text`）
                    blocks.push(crate::model::Block {
                        kind: crate::model::BlockKind::Table,
                        content: t.rows.first().map(|r| r.join(" | ")).unwrap_or_default(),
                        table: Some(t.clone()),
                        meta: it.meta.clone(),
                        // 信物 = 该表格项候选格的连续槽位区间（emit 展开格单元
                        // 时按候选格拆分，见 `emit::to_translation_units_with`）
                        source_token: it.source_token,
                        ..Default::default()
                    });
                    if !it.text.is_empty() {
                        parts.push(it.text.clone());
                    }
                    continue;
                }
                // 空白段落不进块，也不进页文本（Python 侧同样过滤）
                if it.text.trim().is_empty() {
                    continue;
                }
                let mut block = if it.chart {
                    crate::model::Block {
                        kind: crate::model::BlockKind::Chart,
                        content: it.text.clone(),
                        ..Default::default()
                    }
                } else if let Some(lv) = it.level {
                    crate::model::Block {
                        kind: crate::model::BlockKind::Heading,
                        content: it.text.clone(),
                        level: Some(lv.clamp(1, 6)),
                        ..Default::default()
                    }
                } else if let Some(ordered) = it.list {
                    crate::model::Block {
                        kind: crate::model::BlockKind::List,
                        content: it.text.clone(),
                        ordered: Some(ordered),
                        ..Default::default()
                    }
                } else {
                    crate::model::Block {
                        kind: crate::model::BlockKind::Paragraph,
                        content: it.text.clone(),
                        ..Default::default()
                    }
                };
                block.meta = it.meta.clone();
                // 信物透传（1:1 块原样赋给 Block；丢弃的空白 item 信物自然消失）
                block.source_token = it.source_token;
                parts.push(it.text.clone());
                blocks.push(block);
            }
            // 块间用空行分隔（对齐 Python `blocks_to_text` 的 `sep="\n\n"`）；
            // 备注等附属文本不进块，只追加到页文本末尾（Python 默认行为）
            let mut text = parts.join("\n\n");
            if let Some(notes) = rp.notes.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                // 区域块（对齐 Python `policy.include_regions=True`）：备注单独成 Note 块，
                // `meta["region"] = "notes"` 供下游降权；默认关闭时只并入页文本。
                // 已知差异：Python 开启后备注会在页文本里再追加一次（块文本 + 页尾重复），
                // 这里不复制该重复（见 `rust/FEATURES.md` §5）。
                if options.include_regions {
                    let mut meta = std::collections::BTreeMap::new();
                    meta.insert(
                        "region".to_string(),
                        crate::model::MetaValue::from("notes"),
                    );
                    blocks.push(crate::model::Block {
                        kind: crate::model::BlockKind::Note,
                        content: notes.to_string(),
                        meta,
                        ..Default::default()
                    });
                }
                text = if text.is_empty() {
                    notes.to_string()
                } else {
                    format!("{text}\n\n{notes}")
                };
            }
            // 页眉/页脚/脚注/尾注 + 单元格批注：只在 include_regions 下成 Note 块，
            // 且**不进页文本**（与 Python 各 Plugin.assemble 同口径）
            if options.include_regions {
                push_region_notes(&mut blocks, rp);
            }
            crate::model::Page {
                name: rp.name.clone(),
                blocks,
                text,
                images,
                ..Default::default()
            }
        })
        .collect();
    crate::model::Document {
        pages,
        ..Default::default()
    }
}

/// 区域内容 → `Note` 块：docx 页眉/页脚/脚注/尾注（`meta["region"]=kind`）与
/// xlsx 单元格批注（`meta["region"]="comment"` + `meta["cell"]`）。
/// 调用方负责 `include_regions` 判断；区域内容**不进页文本**。
fn push_region_notes(blocks: &mut Vec<crate::model::Block>, rp: &RawPage) {
    for region in &rp.regions {
        if region.text.trim().is_empty() {
            continue;
        }
        let mut meta = std::collections::BTreeMap::new();
        meta.insert(
            "region".to_string(),
            crate::model::MetaValue::from(region.kind.as_str()),
        );
        blocks.push(crate::model::Block {
            kind: crate::model::BlockKind::Note,
            content: region.text.clone(),
            meta,
            ..Default::default()
        });
    }
    for comment in &rp.comments {
        if comment.text.trim().is_empty() {
            continue;
        }
        let mut meta = std::collections::BTreeMap::new();
        meta.insert(
            "cell".to_string(),
            crate::model::MetaValue::from(comment.cell.as_str()),
        );
        meta.insert(
            "region".to_string(),
            crate::model::MetaValue::from("comment"),
        );
        blocks.push(crate::model::Block {
            kind: crate::model::BlockKind::Note,
            content: comment.text.clone(),
            meta,
            ..Default::default()
        });
    }
}

/// 注册 Office 后端（docx/xlsx/pptx raw 文本提取）与插件（默认选项）。
///
/// 只注册编译进来的格式（见 [`FORMATS`]）；被 feature 裁掉的格式不会出现在注册表里，
/// `engine` 遇到该扩展名时按 `unsupported` 返回。
pub fn register_defaults(reg: &mut Registry) {
    register_with(reg, &OfficeOptions::default());
}

/// 同 [`register_defaults`]，但注入组装选项（如 `include_regions` 产出 `BlockKind::Note` 块）。
///
/// 选项走「注册时注入」而不是改 `ParseCapability`/`FormatPlugin` 签名：与 PDF 后端
/// `register_with` 保持同一模式，默认值等价于历史行为。
pub fn register_with(reg: &mut Registry, options: &OfficeOptions) {
    // 注册后端的顺序：docx → xlsx → pptx，由 engine 解析链依次尝试
    #[cfg(feature = "docx")]
    reg.register_parse(
        Arc::new(
            docx::DocxRawBackend::new(options.include_regions)
                .with_structure_mode(options.structure_mode)
                .with_ocr_images(options.ocr_images),
        ),
        None,
    )
    .ok();
    #[cfg(feature = "xlsx")]
    reg.register_parse(
        Arc::new(
            xlsx::XlsxRawBackend::new(options.include_regions)
                .with_truncate(options.truncate_threshold, options.truncate_size)
                .with_max_sheets(options.max_sheets)
                .with_ocr_images(options.ocr_images)
                .with_merge_cell_policy(options.merge_cell_policy),
        ),
        None,
    )
    .ok();
    #[cfg(feature = "pptx")]
    reg.register_parse(
        Arc::new(
            pptx::PptxRawBackend::default()
                .with_truncate(options.truncate_threshold, options.truncate_size)
                .with_max_slides(options.max_slides)
                .with_structure_mode(options.structure_mode)
                .with_ocr_images(options.ocr_images),
        ),
        None,
    )
    .ok();
    reg.register_plugin(Arc::new(OfficePlugin { options: *options }));
}

/// quick-xml 0.42 把 `&amp;` / `&#65;` 这类引用拆成独立的 `Event::GeneralRef`，
/// 不再是 Text 的一部分；这里按 0.37 的语义就地解码，未知实体丢弃（与 unescape 失败一致）。
#[cfg(any(feature = "docx", feature = "xlsx", feature = "pptx"))]
pub(crate) fn push_general_ref(buf: &mut String, e: &quick_xml::events::BytesRef<'_>) {
    match e.resolve_char_ref() {
        Ok(Some(ch)) => buf.push(ch),
        _ => {
            // 命名实体：借 unescape 解 `&name;`（预定义实体表在 quick-xml 内部）
            let entity = format!("&{};", e.xml10_content());
            if let Ok(t) = quick_xml::escape::unescape(&entity) {
                buf.push_str(&t);
            }
        }
    }
}

/// 按属性本地名取值（`w:val` / `r:id` / `ref` 等），命名空间前缀不影响匹配。
#[cfg(any(feature = "docx", feature = "xlsx", feature = "pptx"))]
pub(crate) fn attr_by(e: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.local_name().as_ref() == name)
        .map(|a| a.value.to_string())
}

/// 部件的 `.rels` 路径：`xl/worksheets/sheet1.xml` → `xl/worksheets/_rels/sheet1.xml.rels`
/// （对齐 Python `_rels_path`；根级部件则落在 `_rels/xxx.rels`）。
#[cfg(any(feature = "docx", feature = "xlsx", feature = "pptx"))]
pub(crate) fn part_rels_path(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, base)) => format!("{dir}/_rels/{base}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// 从 rels XML 中按文档序取 `(Id, Type, Target)`（对齐 Python `_read_rels_typed`）。
///
/// 只读属性、命名空间无关（只比本地名）；坏 XML 拿已解析到的部分——
/// rels 属于导航信息，缺一条不该让整份文档失败。
#[cfg(any(feature = "docx", feature = "xlsx", feature = "pptx"))]
pub(crate) fn rel_entries(xml: &str) -> Vec<(String, String, String)> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().local_name().as_ref() == "Relationship" {
                    let mut id = String::new();
                    let mut ty = None;
                    let mut target = None;
                    for attr in e.attributes().filter_map(|a| a.ok()) {
                        match attr.key.local_name().as_ref() {
                            "Id" => id = attr.value.to_string(),
                            "Type" => ty = Some(attr.value.to_string()),
                            "Target" => target = Some(attr.value.to_string()),
                            _ => {}
                        }
                    }
                    if let (Some(ty), Some(target)) = (ty, target) {
                        out.push((id, ty, target));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// rels 的 `Target` 解析：绝对路径去前导 `/`，相对路径按所在部件目录拼接并归一化
/// `./` `../`（对齐 Python `_read_rels_typed` 的 `posixpath.normpath`）。
#[cfg(any(feature = "docx", feature = "xlsx", feature = "pptx"))]
pub(crate) fn resolve_part(dir: &str, target: &str) -> String {
    let joined = if target.starts_with('/') {
        target.trim_start_matches('/').to_string()
    } else {
        format!("{dir}/{target}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// 部件所在目录（`xl/worksheets/sheet1.xml` → `xl/worksheets`）。
#[cfg(any(feature = "docx", feature = "xlsx", feature = "pptx"))]
pub(crate) fn part_dir(part: &str) -> &str {
    part.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

/// 截断预算（对齐 Python `XlsxParseCapability._budget`）：文件 > `threshold` 才启用
/// `size` 预算；任一为 `0` 视为关闭。取不到文件大小时按关闭处理（与 Python 抛错后
/// 走 `0` 的兜底一致，不因为 stat 失败就截断正文）。
#[cfg(any(feature = "xlsx", feature = "pptx"))]
pub(crate) fn truncate_budget(path: &Path, threshold: u64, size: usize) -> usize {
    if threshold == 0 || size == 0 {
        return 0;
    }
    match std::fs::metadata(path) {
        Ok(m) if m.len() > threshold => size,
        _ => 0,
    }
}

/// 表格行 → raw 项：`text` 保留展开文本（页文本层，与 Python `_item_text` 同口径：
/// 行内 `,` 连接、行间 `\n`），`table` 携带行列结构供 `assemble` 产出 `BlockKind::Table`。
/// 调用点只在 docx/pptx 提取路径（xlsx 用带表头版）——docx/pptx 单开时调用点归零，
/// 允许 dead（与 stream.rs 的 `unsupported` 同款处理）。
#[cfg(any(feature = "docx", feature = "xlsx", feature = "pptx"))]
#[allow(dead_code)]
pub(crate) fn table_item(rows: Vec<Vec<String>>) -> crate::plugin::RawTextItem {
    table_item_with_header(rows, 0)
}

/// 同 `table_item`，但声明前 `header_rows` 行为表头（xlsx 冻结窗格用）。
#[cfg(any(feature = "docx", feature = "xlsx", feature = "pptx"))]
pub(crate) fn table_item_with_header(
    rows: Vec<Vec<String>>,
    header_rows: usize,
) -> crate::plugin::RawTextItem {
    let text = rows
        .iter()
        .map(|r| r.join(","))
        .collect::<Vec<_>>()
        .join("\n");
    crate::plugin::RawTextItem {
        text,
        table: Some(crate::model::TableData { rows, header_rows }),
        ..Default::default()
    }
}

/// `RawImage` → IR `Image`：**只带元信息**（`dedup_key` / `ocr_text` / `bbox`），
/// 字节不落 IR（对齐 Python `_to_ir_image` 的 `data=b""`，避免二进制进 JSON）。
/// 去重键优先 md5（与 Python `hashlib.md5` 同值），无字节时退化为 `ext@宽x高`。
fn to_ir_image(im: &crate::plugin::RawImage) -> crate::model::Image {
    let key = if im.data.is_empty() {
        match im.px {
            Some((w, h)) => format!("{}@{w}x{h}", im.ext),
            None => im.ext.clone(),
        }
    } else {
        image::dedup_key(&im.data)
    };
    crate::model::Image {
        bbox: im.bbox,
        dedup_key: Some(key),
        ocr_text: im.ocr_text.clone(),
        ..Default::default()
    }
}

/// 从 ZIP 中读取字节条目（找不到/无法读取返回 None）：图片字节是可选增强，
/// 单个损坏的 media 部件不该拖垮整篇解析。
pub(crate) fn zip_read_bytes(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
) -> Option<Vec<u8>> {
    let mut f = zip.by_name(name).ok()?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut buf).ok()?;
    Some(buf)
}

/// 从 ZIP 中读取字符串条目（找不到返回 None）。
#[cfg(any(feature = "docx", feature = "xlsx"))]
/// 打开一个 OOXML 容器的 `ZipArchive`。
///
/// 并行解析时每个工作块要开自己的一份（`ZipArchive` 读条目需要 `&mut`，
/// 跨线程共享只能加锁 = 假并行）；串行路径上也统一走这里，错误文案只有一份。
pub(crate) fn open_zip(path: &Path) -> Result<zip::ZipArchive<std::fs::File>> {
    let file = std::fs::File::open(path)
        .map_err(|e| DocError::corrupt(format!("cannot open {}: {e}", path.display())))?;
    zip::ZipArchive::new(file).map_err(|e| DocError::corrupt(format!("ZIP open failed: {e}")))
}

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

/// 解析 rels 文件（如 `word/_rels/document.xml.rels`），返回 {id → target} 映射。
#[cfg(feature = "pptx")]
pub(crate) fn parse_rels(xml: &str) -> std::collections::HashMap<String, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut map = std::collections::HashMap::new();
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                // 只读属性，文本裁剪与实体解析在此无关
                if e.name().local_name().as_ref() == "Relationship" {
                    let id = e
                        .attributes()
                        .filter_map(|a| a.ok())
                        .find(|a| a.key.as_ref() == "Id")
                        .map(|a| a.value.to_string());
                    let target = e
                        .attributes()
                        .filter_map(|a| a.ok())
                        .find(|a| a.key.as_ref() == "Target")
                        .map(|a| a.value.to_string());
                    if let (Some(id), Some(target)) = (id, target) {
                        map.insert(id, target);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                // 损坏的 rels 不致命，记录并返回已有结果
                log::warn!("parse_rels 失败: {e}");
                break;
            }
            _ => {}
        }
        buf.clear();
    }
    map
}

#[cfg(test)]
mod tests {
    use super::FORMATS;

    /// `FORMATS` 必须与编译进来的 feature 严格一致（裁剪后不能"报"不存在的格式）。
    #[test]
    fn formats_follow_features() {
        let mut want: Vec<&str> = Vec::new();
        if cfg!(feature = "docx") {
            want.push("docx");
        }
        if cfg!(feature = "xlsx") {
            want.push("xlsx");
        }
        if cfg!(feature = "pptx") {
            want.push("pptx");
        }
        assert_eq!(FORMATS, want.as_slice());
    }

    /// `RawTextItem.level` → `BlockKind::Heading`；无 level 仍是 Paragraph。
    #[test]
    fn assemble_maps_level_to_heading() {
        use super::OfficePlugin;
        use crate::model::BlockKind;
        use crate::plugin::{FormatPlugin, RawDoc, RawPage, RawTextItem};

        let raw = RawDoc {
            pages: vec![RawPage {
                items: vec![
                    RawTextItem {
                        text: "章节".into(),
                        level: Some(2),
                        ..Default::default()
                    },
                    RawTextItem {
                        text: "正文".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let doc = OfficePlugin::default().assemble(&raw);
        let blocks = &doc.pages[0].blocks;
        assert_eq!(blocks[0].kind, BlockKind::Heading);
        assert_eq!(blocks[0].level, Some(2));
        assert_eq!(blocks[1].kind, BlockKind::Paragraph);
    }

    /// `RawTextItem.list` → `BlockKind::List`（ordered 原样透传），标题优先于列表。
    #[test]
    fn assemble_maps_list_flag() {
        use super::OfficePlugin;
        use crate::model::BlockKind;
        use crate::plugin::{FormatPlugin, RawDoc, RawPage, RawTextItem};

        let raw = RawDoc {
            pages: vec![RawPage {
                items: vec![
                    RawTextItem {
                        text: "有序项".into(),
                        list: Some(true),
                        ..Default::default()
                    },
                    RawTextItem {
                        text: "无序项".into(),
                        list: Some(false),
                        ..Default::default()
                    },
                    // 两个都给值时按标题处理（与后端 `raw_item` 的互斥口径一致）
                    RawTextItem {
                        text: "标题优先".into(),
                        level: Some(1),
                        list: Some(true),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let doc = OfficePlugin::default().assemble(&raw);
        let blocks = &doc.pages[0].blocks;
        assert_eq!(blocks[0].kind, BlockKind::List);
        assert_eq!(blocks[0].ordered, Some(true));
        assert_eq!(blocks[1].kind, BlockKind::List);
        assert_eq!(blocks[1].ordered, Some(false));
        assert_eq!(blocks[2].kind, BlockKind::Heading);
    }

    /// 表格项 → `BlockKind::Table`（content 取表头 `" | "` 连接）；
    /// 页文本按 Python `blocks_to_text` 口径：块间空行，表格展开全部行。
    #[test]
    fn assemble_maps_table_and_joins_text() {
        use super::{table_item, OfficePlugin};
        use crate::model::BlockKind;
        use crate::plugin::{FormatPlugin, RawDoc, RawPage, RawTextItem};

        let raw = RawDoc {
            pages: vec![RawPage {
                items: vec![
                    RawTextItem {
                        text: "前言".into(),
                        ..Default::default()
                    },
                    table_item(vec![
                        vec!["姓名".into(), "年龄".into()],
                        vec!["张三".into(), "30".into()],
                    ]),
                    RawTextItem {
                        text: "结语".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let doc = OfficePlugin::default().assemble(&raw);
        let page = &doc.pages[0];
        assert_eq!(page.blocks.len(), 3);
        assert_eq!(page.blocks[1].kind, BlockKind::Table);
        assert_eq!(page.blocks[1].content, "姓名 | 年龄");
        assert_eq!(page.blocks[1].table.as_ref().unwrap().rows.len(), 2);
        assert_eq!(page.text, "前言\n\n姓名,年龄\n张三,30\n\n结语");
    }

    /// 空表（xlsx 空 sheet）仍产出一个空表块，但不进页文本（Python 亦然）。
    #[test]
    fn assemble_keeps_empty_table_block() {
        use super::{table_item, OfficePlugin};
        use crate::model::BlockKind;
        use crate::plugin::{FormatPlugin, RawDoc, RawPage};

        let raw = RawDoc {
            pages: vec![RawPage {
                items: vec![table_item(Vec::new())],
                ..Default::default()
            }],
            ..Default::default()
        };

        let doc = OfficePlugin::default().assemble(&raw);
        let page = &doc.pages[0];
        assert_eq!(page.blocks.len(), 1);
        assert_eq!(page.blocks[0].kind, BlockKind::Table);
        assert_eq!(page.blocks[0].content, "");
        assert_eq!(page.text, "");
    }

    /// 默认选项：备注只并入页文本，不产块（= Python `include_regions=False`）。
    #[test]
    fn notes_go_to_page_text_by_default() {
        use super::OfficePlugin;
        use crate::plugin::{FormatPlugin, RawDoc, RawPage, RawTextItem};

        let raw = RawDoc {
            pages: vec![RawPage {
                name: Some("slide-1".into()),
                items: vec![RawTextItem {
                    text: "正文".into(),
                    ..Default::default()
                }],
                notes: Some("演讲者备注".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        let doc = OfficePlugin::default().assemble(&raw);
        let page = &doc.pages[0];
        assert_eq!(page.blocks.len(), 1, "默认不应产出 Note 块");
        assert_eq!(page.text, "正文\n\n演讲者备注");
    }

    /// `include_regions`：备注另产 `BlockKind::Note` 块并标注 `meta["region"]="notes"`
    /// （= Python `policy.include_regions=True` 的块结构）。
    ///
    /// 已知差异：Python 开启后会把备注在页文本里再追加一次（块文本 + 页尾），本实现不复制该重复，
    /// 因此页文本里备注只出现一次。
    #[test]
    fn include_regions_emits_note_block() {
        use super::{OfficeOptions, OfficePlugin};
        use crate::model::{BlockKind, MetaValue};
        use crate::plugin::{FormatPlugin, RawDoc, RawPage, RawTextItem};

        let raw = RawDoc {
            pages: vec![RawPage {
                name: Some("slide-1".into()),
                items: vec![RawTextItem {
                    text: "正文".into(),
                    ..Default::default()
                }],
                notes: Some("演讲者备注".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let plugin = OfficePlugin {
            options: OfficeOptions::new().with_include_regions(true),
        };

        let doc = plugin.assemble(&raw);
        let page = &doc.pages[0];
        assert_eq!(page.blocks.len(), 2);
        assert_eq!(page.blocks[1].kind, BlockKind::Note);
        assert_eq!(page.blocks[1].content, "演讲者备注");
        assert_eq!(
            page.blocks[1].meta.get("region"),
            Some(&MetaValue::Str("notes".to_string()))
        );
        assert_eq!(page.text.matches("演讲者备注").count(), 1, "页文本不重复");
    }

    /// 空备注不产 Note 块（与默认路径同样按 trim 过滤）。
    #[test]
    fn include_regions_skips_blank_notes() {
        use super::{OfficeOptions, OfficePlugin};
        use crate::plugin::{FormatPlugin, RawDoc, RawPage, RawTextItem};

        let raw = RawDoc {
            pages: vec![RawPage {
                items: vec![RawTextItem {
                    text: "正文".into(),
                    ..Default::default()
                }],
                notes: Some("   \n ".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let plugin = OfficePlugin {
            options: OfficeOptions::new().with_include_regions(true),
        };

        let doc = plugin.assemble(&raw);
        assert_eq!(doc.pages[0].blocks.len(), 1);
        assert_eq!(doc.pages[0].text, "正文");
    }

    /// docx 区域（页眉/页脚/脚注）：`include_regions` 下追加 Note 块并标 `region`，
    /// **不进页文本**；默认（不开开关）区域既不产块也不进文本。
    #[test]
    fn include_regions_emits_docx_region_blocks() {
        use super::{OfficeOptions, OfficePlugin};
        use crate::model::{BlockKind, MetaValue};
        use crate::plugin::{FormatPlugin, RawDoc, RawPage, RawRegion, RawTextItem};

        let raw = RawDoc {
            pages: vec![RawPage {
                items: vec![RawTextItem {
                    text: "正文".into(),
                    ..Default::default()
                }],
                regions: vec![
                    RawRegion {
                        kind: "header".into(),
                        text: "页眉内容".into(),
                    },
                    RawRegion {
                        kind: "footnote".into(),
                        text: "脚注文本".into(),
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let on = OfficePlugin {
            options: OfficeOptions::new().with_include_regions(true),
        };
        let page = &on.assemble(&raw).pages[0];
        assert_eq!(page.blocks.len(), 3);
        assert_eq!(page.blocks[1].kind, BlockKind::Note);
        assert_eq!(page.blocks[1].content, "页眉内容");
        assert_eq!(
            page.blocks[1].meta.get("region"),
            Some(&MetaValue::Str("header".into()))
        );
        assert_eq!(
            page.blocks[2].meta.get("region"),
            Some(&MetaValue::Str("footnote".into()))
        );
        assert_eq!(page.text, "正文", "区域不进页文本");

        let off = OfficePlugin::default().assemble(&raw);
        assert_eq!(off.pages[0].blocks.len(), 1);
        assert_eq!(off.pages[0].text, "正文");
    }

    /// xlsx 单元格批注：`include_regions` 下追加 Note 块（`meta` 带 `cell` + `region`），
    /// 页文本只由表格块决定；默认不产块。
    #[test]
    fn include_regions_emits_xlsx_comment_blocks() {
        use super::{OfficeOptions, OfficePlugin};
        use crate::model::{BlockKind, MetaValue, TableData};
        use crate::plugin::{FormatPlugin, RawComment, RawDoc, RawPage, RawTextItem};

        let table = TableData {
            rows: vec![vec!["员工".into()], vec!["张三".into()]],
            header_rows: 1,
        };
        let raw = RawDoc {
            pages: vec![RawPage {
                name: Some("员工".into()),
                items: vec![RawTextItem {
                    text: table.rows[0].join(","),
                    table: Some(table),
                    ..Default::default()
                }],
                comments: vec![RawComment {
                    cell: "A2".into(),
                    text: "批注一".into(),
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let on = OfficePlugin {
            options: OfficeOptions::new().with_include_regions(true),
        };
        let page = &on.assemble(&raw).pages[0];
        assert_eq!(page.blocks.len(), 2);
        let note = &page.blocks[1];
        assert_eq!(note.kind, BlockKind::Note);
        assert_eq!(note.content, "批注一");
        assert_eq!(
            note.meta.get("cell"),
            Some(&MetaValue::Str("A2".into())),
            "批注块必须能定位回单元格"
        );
        assert_eq!(
            note.meta.get("region"),
            Some(&MetaValue::Str("comment".into()))
        );
        assert_eq!(page.text, "员工", "批注与表头都不含批注文本");

        let off = OfficePlugin::default().assemble(&raw);
        assert_eq!(off.pages[0].blocks.len(), 1);
        assert_eq!(off.pages[0].text, "员工");
    }

    /// 快路径（`flat_text`）：逐行产 `Paragraph` 块，空行不产块（与 txt 插件
    /// "空行不作为块"同口径）；`Page.text` 仍取 flat 原串（不按块重算）。
    ///
    /// 这是消费者可用性痛点回归：极简 docx/pptx 曾因快路径零块被
    /// `to_translation_units` 判为"无可翻译文本单元"。
    #[test]
    fn flat_text_produces_paragraph_blocks_per_line() {
        use super::OfficePlugin;
        use crate::model::BlockKind;
        use crate::plugin::{FormatPlugin, RawDoc, RawPage};

        let raw = RawDoc {
            pages: vec![RawPage {
                flat_text: Some("甲行\n\n乙行\n".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        let doc = OfficePlugin::default().assemble(&raw);
        let page = &doc.pages[0];
        assert_eq!(page.blocks.len(), 2, "空行不产块");
        assert!(page.blocks.iter().all(|b| b.kind == BlockKind::Paragraph));
        assert_eq!(page.blocks[0].content, "甲行");
        assert_eq!(page.blocks[1].content, "乙行");
        assert_eq!(page.text, "甲行\n\n乙行\n", "页文本保持 flat 原串");
    }

    /// 快路径 + `include_regions`：区域 `Note` 块行为不变，且排在正文行块之后
    /// （先正文、后区域），`Page.text` 同样不受区域影响。
    #[test]
    fn flat_text_keeps_region_notes_after_paragraphs() {
        use super::{OfficeOptions, OfficePlugin};
        use crate::model::{BlockKind, MetaValue};
        use crate::plugin::{FormatPlugin, RawDoc, RawPage, RawRegion};

        let raw = RawDoc {
            pages: vec![RawPage {
                flat_text: Some("甲行\n乙行".into()),
                regions: vec![RawRegion {
                    kind: "header".into(),
                    text: "页眉内容".into(),
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let plugin = OfficePlugin {
            options: OfficeOptions::new().with_include_regions(true),
        };

        let page = &plugin.assemble(&raw).pages[0];
        assert_eq!(page.blocks.len(), 3);
        assert_eq!(page.blocks[0].kind, BlockKind::Paragraph);
        assert_eq!(page.blocks[1].kind, BlockKind::Paragraph);
        assert_eq!(page.blocks[2].kind, BlockKind::Note);
        assert_eq!(page.blocks[2].content, "页眉内容");
        assert_eq!(
            page.blocks[2].meta.get("region"),
            Some(&MetaValue::Str("header".into()))
        );
        assert_eq!(page.text, "甲行\n乙行", "区域不进页文本");

        // 默认关：只有正文行块
        let off = OfficePlugin::default().assemble(&raw);
        assert_eq!(off.pages[0].blocks.len(), 2);
    }
}
