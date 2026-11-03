// 吸收自 docparser-office 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：crate 引用重写（docparser_core→docforge、office 内部→super）。
//! PPTX 解析：ZIP + quick-xml 流式文本提取（与 Python `plugins/ooxml.py` 的 pptx 侧对齐）。
//!
//! 架构：`PptxRawBackend` -> `ParseCapability`。
//! 每个形状单遍扫描出「文本 + 几何 + 占位符类型 + 表格 + 图表引用 + 项目符号」，
//! 再按几何坐标排序（上→下、左→右）产出 raw 项：
//! 标题占位符 → Heading(1)，项目符号 → List(ordered = 是否 autoNum)，
//! `a:tbl` → Table，`c:chart` → Chart，其余 → Paragraph；
//! 备注页文本只追加到页文本，不进块（与 Python 默认行为一致）。

use crate::error::{DocError, Result};
use crate::plugin::{ParseCapability, RawDoc, RawPage, RawTextItem};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::parse_rels;
use super::structure::StructureMode;
use std::collections::HashMap;

/// PPTX raw 后端。
#[derive(Default)]
pub struct PptxRawBackend {
    /// 截断阈值（字节，`0` = 关闭）：文件大于它才启用 `truncate_size` 预算
    /// （对齐 Python `policy.truncate_threshold`）。
    pub truncate_threshold: u64,
    /// 截断预算（字符数，`0` = 关闭）：按页累计（对齐 Python `truncate_size`）。
    pub truncate_size: usize,
    /// 幻灯片上限（`0` = 不限）：超出丢弃且**不**置 `truncated`
    /// （对齐 Python `policy.max_slides`：列表切片后丢弃）。
    pub max_slides: usize,
    /// 结构还原三态（对齐 Python `policy.structure_mode`）：非语义时**逐页**判断，
    /// 该页走行文本快路径（`flat_text`）、组装层按行产 `Paragraph` 块、备注不读。
    pub structure_mode: StructureMode,
    /// 是否提取内嵌位图（对齐 Python `policy.ocr_images`，默认关）。
    /// 注意：与 Python 一致，raw 页**不**提取图片（`_slide_items` 的快路径直接 return）。
    pub ocr_images: bool,
}

impl PptxRawBackend {
    /// 设置截断预算（`0` = 关闭；语义同 Python `_budget`）。
    pub fn with_truncate(mut self, threshold: u64, size: usize) -> Self {
        self.truncate_threshold = threshold;
        self.truncate_size = size;
        self
    }

    /// 设置幻灯片上限（`0` = 不限；语义同 Python `policy.max_slides`）。
    pub fn with_max_slides(mut self, max: usize) -> Self {
        self.max_slides = max;
        self
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
}

impl ParseCapability for PptxRawBackend {
    fn id(&self) -> &str {
        "office-raw"
    }
    fn formats(&self) -> &[&str] {
        &["pptx"]
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let mut zip = super::open_zip(path)?;

        // 全量缓存所有 XML 和 rels 文件，避免 per-slide 多次 ZIP 遍历
        let mut cache: HashMap<String, String> = HashMap::new();
        for i in 0..zip.len() {
            let mut f = zip
                .by_index(i)
                .map_err(|e| DocError::corrupt(format!("ZIP entry error at index {i}: {e}")))?;
            // zip 9：条目名改为 Result<Cow<str>, ZipError>（非法 UTF-8 名不再静默）
            let name = f
                .name()
                .map_err(|e| DocError::corrupt(format!("ZIP entry name error at index {i}: {e}")))?
                .to_string();
            if name.ends_with(".xml") || name.ends_with(".rels") {
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut f, &mut buf)
                    .map_err(|e| DocError::corrupt(format!("读取 {name} 失败: {e}")))?;
                let s = String::from_utf8(buf)
                    .map_err(|e| DocError::corrupt(format!("{name} 不是 UTF-8: {e}")))?;
                cache.insert(name, s);
            }
        }

        // 读取 slide 列表
        let pres_xml = cache
            .get("ppt/presentation.xml")
            .ok_or_else(|| DocError::corrupt("missing ppt/presentation.xml"))?;
        let slide_rids = parse_slide_list(pres_xml)?;

        // 读取 rels 获取 slide 路径
        let pres_rels = cache
            .get("ppt/_rels/presentation.xml.rels")
            .map(|s| s.as_str())
            .unwrap_or("");
        let rels_map = parse_rels(pres_rels);

        // slide 顺序：优先 sldIdLst（文档真实顺序），回退文件名序号
        // （对齐 Python `_slide_order`：sldIdLst 为空或 rels 缺失时按 slideN 排序）
        let mut slides: Vec<(usize, String)> = slide_rids
            .iter()
            .filter_map(|rid| {
                let target = rels_map.get(rid)?;
                // target 可能是 "slides/slide1.xml" 或 "./slides/slide1.xml"
                let cleaned = target.trim_start_matches("./");
                Some(format!("ppt/{}", cleaned))
            })
            .enumerate()
            .map(|(i, path)| (i + 1, path))
            .collect();
        if slides.is_empty() {
            slides = slides_by_filename(&cache);
        }
        if slides.is_empty() {
            return Err(DocError::corrupt("no slides found in presentation.xml"));
        }
        // 幻灯片上限：切片保留原序号，且**不**置 `truncated`
        // （对齐 Python `slides = slides[:max_slides]`）
        if self.max_slides > 0 {
            slides.truncate(self.max_slides);
        }

        // 截断预算（对齐 Python `_budget`）：按页累计，超预算的**这一页整页保留**、
        // 备注不再读，之后的幻灯片不再解析（Python：`used += ln` 后仍 append 再 break）
        let budget = super::truncate_budget(path, self.truncate_threshold, self.truncate_size);
        let mut used = 0usize;
        let mut truncated = false;

        // 解析每个 slide。
        //
        // 并行只在不开预算时启用（预算按页累计、命中即停，并行会把后续页白解析一遍）；
        // 每块按需开自己的 ZIP（只有 `ocr_images` 才需要读 media 部件）。
        let mut pages: Vec<RawPage> = Vec::with_capacity(slides.len());
        if budget == 0 && slides.len() > 1 {
            let contents = super::parallel::map_chunks_ordered(slides.clone(), |chunk| {
                let mut z = if self.ocr_images {
                    Some(super::open_zip(path)?)
                } else {
                    None
                };
                chunk
                    .iter()
                    .map(|(page_num, slide_path)| {
                        parse_slide_content(
                            &cache,
                            slide_path,
                            *page_num,
                            self.structure_mode,
                            z.as_mut(),
                        )
                    })
                    .collect()
            })?;
            for ((page_num, slide_path), content) in slides.iter().zip(contents) {
                pages.push(slide_page(
                    *page_num,
                    slide_path,
                    &cache,
                    content,
                    &mut used,
                    budget,
                    &mut truncated,
                ));
            }
        } else {
            for (page_num, slide_path) in slides.iter() {
                let content = parse_slide_content(
                    &cache,
                    slide_path,
                    *page_num,
                    self.structure_mode,
                    if self.ocr_images {
                        Some(&mut zip)
                    } else {
                        None
                    },
                )?;
                pages.push(slide_page(
                    *page_num,
                    slide_path,
                    &cache,
                    content,
                    &mut used,
                    budget,
                    &mut truncated,
                ));
                if truncated {
                    break;
                }
            }
        }

        Ok(RawDoc {
            pages,
            ..Default::default()
        })
    }
}

/// 单页解析产物（预算与备注取舍留给调用方，保持「顺序语义只有一处」）。
struct SlideContent {
    items: Vec<RawTextItem>,
    flat_lines: Vec<String>,
    /// 该页 rels：备注页/图表都按 r:id 解析（不猜文件名）
    rels: HashMap<String, String>,
    /// 是否走了结构还原（非语义页的 items/flat_lines 二选一，组装时要看这个标志）
    semantic: bool,
}

/// 解析一页：形状 → 几何定序 → 语义/快路径二选一。
///
/// `zip` 只在需要读 media（`ocr_images`）时由调用方给出；并行路径下每块各有一份。
fn parse_slide_content(
    cache: &HashMap<String, String>,
    slide_path: &str,
    page_num: usize,
    structure_mode: StructureMode,
    zip: Option<&mut zip::ZipArchive<std::fs::File>>,
) -> Result<SlideContent> {
    let xml = cache.get(slide_path).cloned().unwrap_or_default();
    let mut shapes = extract_shapes(&xml)?;

    // 从文件路径 "ppt/slides/slideN.xml" 提取 N（用于定位 rels）
    let slide_num: usize = slide_path
        .trim_start_matches("ppt/slides/slide")
        .trim_end_matches(".xml")
        .parse()
        .unwrap_or(page_num);

    // slide rels：备注页与图表都按 r:id 从这里解析（不猜文件名）
    let rels_path = format!("ppt/slides/_rels/slide{}.xml.rels", slide_num);
    let rels = cache
        .get(&rels_path)
        .map(|s| parse_rels(s))
        .unwrap_or_default();

    // cheap probe（**逐页**判定，对齐 Python）：多形状 / 表格 / 图表 →
    // 值得做结构还原；auto 模式下单形状纯文本页走 raw 快路径。
    let has_struct = shapes.len() > 1
        || shapes
            .iter()
            .any(|s| s.rows.is_some() || s.chart_rid.is_some());
    let semantic = structure_mode.resolve(has_struct);

    let (items, flat_lines) = if semantic {
        // 每页一个图片读取器：去重只在页内生效（对齐 Python `seen_media`）
        let mut images = zip.map(super::image::ImageReader::new);
        (
            build_slide_items(&mut shapes, cache, &rels, slide_path, images.as_mut()),
            Vec::new(),
        )
    } else {
        (
            Vec::new(),
            build_flat_lines(&shapes, cache, &rels, slide_path),
        )
    };

    Ok(SlideContent {
        items,
        flat_lines,
        rels,
        semantic,
    })
}

/// 应用预算、决定备注、组装成页（`used` / `truncated` 是跨页累计量，由调用方持有）。
fn slide_page(
    page_num: usize,
    slide_path: &str,
    cache: &HashMap<String, String>,
    content: SlideContent,
    used: &mut usize,
    budget: usize,
    truncated: &mut bool,
) -> RawPage {
    let SlideContent {
        items,
        flat_lines,
        rels,
        semantic,
    } = content;
    if budget > 0 {
        // 预算按 Python `_item_text` 逐项累计（行间 `\n` 不计入）
        let len: usize = if semantic {
            items.iter().map(|i| i.text.chars().count()).sum()
        } else {
            flat_lines.iter().map(|t| t.chars().count()).sum()
        };
        if *used + len > budget {
            *truncated = true;
        }
        *used += len;
    }
    // 命中预算的那一页不取备注（Python：`want_notes and not truncated`）；
    // 非语义模式下备注整体丢弃（Python `want_notes = semantic or include_regions`，
    // 而 assemble 的非语义分支根本不看 notes）——连读都不读
    let notes = if *truncated || !semantic {
        None
    } else {
        notes_text(cache, &rels, slide_path)
    };

    // 页名与 Python 一致：sldId 顺序 → 序号；文件名回退 → 文件名里的数字
    RawPage {
        name: Some(format!("slide-{}", page_num)),
        items,
        flat_text: if semantic {
            None
        } else {
            Some(flat_lines.join("\n"))
        },
        notes,
        ..Default::default()
    }
}

/// 解析 `<sldIdLst>` 获取 slide 的 r:id 列表（保持文档顺序）。
fn parse_slide_list(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    // 不裁剪文本：空白由消费者 trim（实体两侧空格需保留，见 GeneralRef 分支）
    let mut buf = Vec::new();
    let mut rids = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                if e.name().local_name().as_ref() == super::tags::P_SLDID {
                    for attr in e.attributes().filter_map(|a| a.ok()) {
                        // 属性按完整 QName 比较：只认 r:id，`sldId` 自己的裸 id 不算关系 id
                        if attr.key.as_ref() == "r:id" {
                            rids.push(attr.value.to_string());
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().local_name().as_ref() == "presentation" {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("presentation parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(rids)
}

/// 文件名回退：`ppt/slides/slideN.xml` 按 N 排序（对齐 Python `_slide_order` 的回退分支）。
fn slides_by_filename(cache: &HashMap<String, String>) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = cache
        .keys()
        .filter_map(|name| {
            let rest = name.strip_prefix("ppt/slides/slide")?;
            let num = rest.strip_suffix(".xml")?;
            // 只认纯数字：排除 slideLayout / slideMaster 之类
            let n: usize = num.parse().ok()?;
            Some((n, name.clone()))
        })
        .collect();
    out.sort_by_key(|(n, _)| *n);
    out
}

/// 单个形状的单遍扫描结果（对齐 Python `_ShapeInfo` / `shape_info`）。
#[derive(Debug, Default)]
struct ShapeInfo {
    /// 形状内全部 `a:t` 文本，`a:br` 记作换行
    text: String,
    /// 首个 `a:off` 的 (x, y)
    xy: Option<(i64, i64)>,
    /// 首个 `p:ph` 的 type（缺省 type 记空串）
    ph: Option<String>,
    /// 首个 `a:tbl` 的行列结构
    rows: Option<Vec<Vec<String>>>,
    /// 首个 `c:chart` 的 r:id
    chart_rid: Option<String>,
    bullet_char: bool,
    bullet_num: bool,
    /// 形状内的图片引用 id（`a:blip` / `v:imagedata`，文档序，去重）
    /// 对齐 Python `_blip_rids(info.el)`。
    rids: Vec<String>,
}

impl ShapeInfo {
    /// 是否为标题占位符（Python：`ph in ("title", "ctrTitle")`）。
    fn is_title(&self) -> bool {
        matches!(self.ph.as_deref(), Some("title") | Some("ctrTitle"))
    }
}

/// 按本地名取属性值（`type` / `id` / `x` / `y`）。
fn attr_local(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.local_name().as_ref() == name)
        .map(|a| a.value.to_string())
}

/// 读 `a:off` 的 x / y（任一非法值即视为无坐标，与 Python `int()` 失败一致）。
fn read_xy(e: &BytesStart<'_>) -> Option<(i64, i64)> {
    let mut x = 0i64;
    let mut y = 0i64;
    for a in e.attributes().filter_map(|a| a.ok()) {
        let v = a.value.to_string();
        match a.key.local_name().as_ref() {
            "x" => x = v.trim().parse().ok()?,
            "y" => y = v.trim().parse().ok()?,
            _ => {}
        }
    }
    Some((x, y))
}

/// 提取 slide 内的形状（`p:sp` / `p:pic` / `p:graphicFrame` / `p:cxnSp`），
/// 组合形状递归：组内每个子形状各算一项（与 Python `_collect_shapes` 一致）。
fn extract_shapes(xml: &str) -> Result<Vec<ShapeInfo>> {
    let mut reader = Reader::from_str(xml);
    // 不裁剪文本：空白由消费者 trim（实体两侧空格需保留，见 GeneralRef 分支）
    let mut buf = Vec::new();
    let mut shapes: Vec<ShapeInfo> = Vec::new();
    // 形状只可能通过 p:grpSp 嵌套，而 grpSp 本身不是形状，故用栈即可
    let mut stack: Vec<ShapeInfo> = Vec::new();

    let mut in_t = false;
    let mut text_buf = String::new();

    // 表格状态（表格不嵌套；仅服务当前最内层形状）
    let mut in_tbl = false;
    let mut in_tr = false;
    let mut in_tc = false;
    let mut cell = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let name = name.local_name();
                // 图片引用：记在**最内层**形状上（嵌套形状/组合各自算，同 Python
                // `_blip_rids(info.el)` 的逐形状语义）
                if let Some(rid) = super::image::rid_of_image_node(e) {
                    if let Some(cur) = stack.last_mut() {
                        if !cur.rids.contains(&rid) {
                            cur.rids.push(rid);
                        }
                    }
                }
                match name.as_ref() {
                    super::tags::P_SP
                    | super::tags::P_PIC
                    | super::tags::P_GFRAME
                    | super::tags::P_CXNSP => stack.push(ShapeInfo::default()),
                    super::tags::A_OFF => {
                        if let Some(cur) = stack.last_mut() {
                            if cur.xy.is_none() {
                                cur.xy = read_xy(e);
                            }
                        }
                    }
                    super::tags::P_PH => {
                        if let Some(cur) = stack.last_mut() {
                            if cur.ph.is_none() {
                                cur.ph = Some(attr_local(e, "type").unwrap_or_default());
                            }
                        }
                    }
                    super::tags::C_CHART => {
                        if let Some(cur) = stack.last_mut() {
                            if cur.chart_rid.is_none() {
                                cur.chart_rid = attr_local(e, "id").filter(|s| !s.is_empty());
                            }
                        }
                    }
                    super::tags::A_BUCHAR => {
                        if let Some(cur) = stack.last_mut() {
                            cur.bullet_char = true;
                        }
                    }
                    super::tags::A_BUAUTONUM => {
                        if let Some(cur) = stack.last_mut() {
                            cur.bullet_num = true;
                        }
                    }
                    super::tags::A_T => {
                        in_t = true;
                        text_buf.clear();
                    }
                    super::tags::A_BR => {
                        if let Some(cur) = stack.last_mut() {
                            cur.text.push('\n');
                        }
                    }
                    super::tags::A_TBL => {
                        in_tbl = true;
                        rows.clear();
                    }
                    super::tags::A_TR => {
                        in_tr = true;
                        row.clear();
                    }
                    super::tags::A_TC => {
                        in_tc = true;
                        cell.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.name();
                let name = name.local_name();
                if let Some(rid) = super::image::rid_of_image_node(e) {
                    if let Some(cur) = stack.last_mut() {
                        if !cur.rids.contains(&rid) {
                            cur.rids.push(rid);
                        }
                    }
                }
                match name.as_ref() {
                    // 自闭合形状：无内容，直接成形
                    super::tags::P_SP
                    | super::tags::P_PIC
                    | super::tags::P_GFRAME
                    | super::tags::P_CXNSP => shapes.push(ShapeInfo::default()),
                    super::tags::A_OFF => {
                        if let Some(cur) = stack.last_mut() {
                            if cur.xy.is_none() {
                                cur.xy = read_xy(e);
                            }
                        }
                    }
                    super::tags::P_PH => {
                        if let Some(cur) = stack.last_mut() {
                            if cur.ph.is_none() {
                                cur.ph = Some(attr_local(e, "type").unwrap_or_default());
                            }
                        }
                    }
                    super::tags::C_CHART => {
                        if let Some(cur) = stack.last_mut() {
                            if cur.chart_rid.is_none() {
                                cur.chart_rid = attr_local(e, "id").filter(|s| !s.is_empty());
                            }
                        }
                    }
                    super::tags::A_BUCHAR => {
                        if let Some(cur) = stack.last_mut() {
                            cur.bullet_char = true;
                        }
                    }
                    super::tags::A_BUAUTONUM => {
                        if let Some(cur) = stack.last_mut() {
                            cur.bullet_num = true;
                        }
                    }
                    super::tags::A_BR => {
                        if let Some(cur) = stack.last_mut() {
                            cur.text.push('\n');
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                let name = name.local_name();
                match name.as_ref() {
                    super::tags::P_SP
                    | super::tags::P_PIC
                    | super::tags::P_GFRAME
                    | super::tags::P_CXNSP => {
                        if let Some(s) = stack.pop() {
                            shapes.push(s);
                        }
                    }
                    super::tags::A_T => {
                        in_t = false;
                        if let Some(cur) = stack.last_mut() {
                            cur.text.push_str(&text_buf);
                        }
                        if in_tc {
                            cell.push_str(&text_buf);
                        }
                        text_buf.clear();
                    }
                    super::tags::A_TC => {
                        if in_tc {
                            row.push(cell.trim().to_string());
                            cell.clear();
                            in_tc = false;
                        }
                    }
                    super::tags::A_TR => {
                        if in_tr {
                            if !row.is_empty() {
                                rows.push(std::mem::take(&mut row));
                            }
                            in_tr = false;
                        }
                    }
                    super::tags::A_TBL if in_tbl => {
                        in_tbl = false;
                        match stack.last_mut() {
                            // 只保留首个表格（Python 取第一个 a:tbl）
                            Some(cur) if cur.rows.is_none() => {
                                cur.rows = Some(std::mem::take(&mut rows));
                            }
                            _ => rows.clear(),
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_t {
                    // quick-xml 0.38+：Text 已是解码后的字符串，实体需显式 unescape
                    let raw = e.xml10_content();
                    if let Ok(t) = quick_xml::escape::unescape(&raw) {
                        text_buf.push_str(&t);
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if in_t {
                    super::push_general_ref(&mut text_buf, e);
                }
            }
            Ok(Event::CData(ref e)) => {
                if in_t {
                    text_buf.push_str(e.as_ref());
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("slide parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    Ok(shapes)
}

/// 非语义（raw）模式的每页行文本：**保持文档序**（不做几何排序）、不做标题/列表判定、
/// 表格压平成一段文本、图表文本按普通段落产出（对齐 Python `_slide_items` 的
/// `not semantic` 分支）。返回逐项文本：预算按项累计、页文本按 `\n` 拼接。
fn build_flat_lines(
    shapes: &[ShapeInfo],
    cache: &HashMap<String, String>,
    rels: &HashMap<String, String>,
    slide_path: &str,
) -> Vec<String> {
    let mut lines = Vec::new();
    for s in shapes {
        if let Some(rows) = &s.rows {
            if !rows.is_empty() {
                lines.push(
                    rows.iter()
                        .map(|r| r.join(","))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
        if let Some(rid) = s.chart_rid.as_deref() {
            if let Some(text) = chart_text(cache, rels, slide_path, rid) {
                lines.push(text);
            }
        }
        // 表格/图表形状不再输出平铺文本（与 semantic 分支同一取舍）
        if s.rows.is_some() || s.chart_rid.is_some() {
            continue;
        }
        let txt = s.text.trim();
        if !txt.is_empty() {
            lines.push(txt.to_string());
        }
    }
    lines
}

/// 形状序列 → raw 项（几何排序 + 类型判定，对齐 Python `_slide_items` 的 semantic 分支）。
/// `images` 只在 `ocr_images` 时给出（每页一个读取器 = 每页去重）。
fn build_slide_items(
    shapes: &mut [ShapeInfo],
    cache: &HashMap<String, String>,
    rels: &HashMap<String, String>,
    slide_path: &str,
    mut images: Option<&mut super::image::ImageReader<'_>>,
) -> Vec<RawTextItem> {
    // 几何排序（上→下、左→右）；坐标缺失且过半的形状没有坐标时保持文档序
    let with_xy = shapes.iter().filter(|s| s.xy.is_some()).count();
    if with_xy > 0 && with_xy >= (shapes.len() / 2).max(1) {
        shapes.sort_by_key(|s| match s.xy {
            Some((x, y)) => (0, y, x),
            None => (1, 0, 0),
        });
    }

    let mut items = Vec::new();
    for s in shapes.iter() {
        if let Some(rows) = &s.rows {
            if !rows.is_empty() {
                items.push(super::table_item(rows.clone()));
            }
        }
        if let Some(rid) = s.chart_rid.as_deref() {
            if let Some(text) = chart_text(cache, rels, slide_path, rid) {
                items.push(RawTextItem {
                    text,
                    chart: true,
                    ..Default::default()
                });
            }
        }
        // 表格/图表形状不再输出平铺文本（否则单元格文字会与结构化表格重复）
        if s.rows.is_some() || s.chart_rid.is_some() {
            continue;
        }

        let txt = s.text.trim();
        if !txt.is_empty() {
            let item = if s.is_title() {
                RawTextItem {
                    text: txt.to_string(),
                    level: Some(1),
                    ..Default::default()
                }
            } else if s.bullet_char || s.bullet_num {
                RawTextItem {
                    text: txt.to_string(),
                    list: Some(s.bullet_num),
                    ..Default::default()
                }
            } else {
                RawTextItem {
                    text: txt.to_string(),
                    ..Default::default()
                }
            };
            items.push(item);
        }

        // 内嵌位图：只在文本形状上找（表格/图表形状上面已 continue），
        // 每页按部件去重（Python `seen_media`），图片项排在文本项之后
        let Some(reader) = images.as_mut() else {
            continue;
        };
        for rid in s.rids.iter() {
            let Some(target) = rels.get(rid) else {
                continue;
            };
            let part = resolve_rels_target(slide_path, target);
            if let Some(im) = reader.load(&part) {
                items.push(super::image::image_item(im));
            }
        }
    }
    items
}

/// 图表文本：`c:chart` 的 r:id → slide rels → 图表 XML。
fn chart_text(
    cache: &HashMap<String, String>,
    rels: &HashMap<String, String>,
    slide_path: &str,
    rid: &str,
) -> Option<String> {
    let target = rels.get(rid)?;
    let path = resolve_rels_target(slide_path, target);
    let xml = cache.get(&path)?;
    let text = chart_title_and_labels(xml);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 图表内文本：首个 `c:title` 的文本 + 每个 `c:dLbl` 的文本
/// （`a:t`，缺失时退回 `c:v`），行间 `\n`。对齐 Python `_chart`。
fn chart_title_and_labels(xml: &str) -> String {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut parts: Vec<String> = Vec::new();

    let mut title_depth = 0usize;
    let mut title_done = false;
    let mut title_buf = String::new();
    let mut dlbl_depth = 0usize;
    let mut dlbl_a = String::new();
    let mut dlbl_v = String::new();

    let mut in_t = false;
    let mut in_v = false;
    let mut text_buf = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let name = name.local_name();
                match name.as_ref() {
                    super::tags::C_TITLE if !title_done => {
                        title_depth += 1;
                        title_buf.clear();
                    }
                    super::tags::C_DLBL => {
                        if dlbl_depth == 0 {
                            dlbl_a.clear();
                            dlbl_v.clear();
                        }
                        dlbl_depth += 1;
                    }
                    super::tags::A_T => {
                        in_t = true;
                        text_buf.clear();
                    }
                    super::tags::C_V if dlbl_depth > 0 => {
                        in_v = true;
                        text_buf.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                let name = name.local_name();
                match name.as_ref() {
                    super::tags::A_T => {
                        in_t = false;
                        if title_depth > 0 {
                            title_buf.push_str(&text_buf);
                        } else if dlbl_depth > 0 {
                            dlbl_a.push_str(&text_buf);
                        }
                        text_buf.clear();
                    }
                    super::tags::C_V => {
                        if in_v {
                            in_v = false;
                            if dlbl_depth > 0 {
                                dlbl_v.push_str(&text_buf);
                            }
                            text_buf.clear();
                        }
                    }
                    super::tags::C_DLBL => {
                        if dlbl_depth > 0 {
                            dlbl_depth -= 1;
                            if dlbl_depth == 0 {
                                let t = if dlbl_a.trim().is_empty() {
                                    dlbl_v.trim()
                                } else {
                                    dlbl_a.trim()
                                };
                                if !t.is_empty() {
                                    parts.push(t.to_string());
                                }
                            }
                        }
                    }
                    super::tags::C_TITLE if title_depth > 0 => {
                        title_depth -= 1;
                        if title_depth == 0 {
                            title_done = true;
                            let t = title_buf.trim();
                            if !t.is_empty() {
                                parts.push(t.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_t || in_v {
                    let raw = e.xml10_content();
                    if let Ok(t) = quick_xml::escape::unescape(&raw) {
                        text_buf.push_str(&t);
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if in_t || in_v {
                    super::push_general_ref(&mut text_buf, e);
                }
            }
            Ok(Event::CData(ref e)) => {
                if in_t || in_v {
                    text_buf.push_str(e.as_ref());
                }
            }
            Ok(Event::Eof) => break,
            // 损坏的图表不阻断整篇（Python `except` 后返回空文本）
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    parts.join("\n")
}

/// 备注页文本：slide rels 指向的 notesSlide 里第一个 `body` 占位符的文本。
///
/// 跳过 sldNum / sldImg / dt / ftr / hdr 等占位符（对齐 Python `_notes`）。
fn notes_text(
    cache: &HashMap<String, String>,
    rels: &HashMap<String, String>,
    slide_path: &str,
) -> Option<String> {
    for target in rels.values() {
        if !target.contains("notesSlide") {
            continue;
        }
        let path = resolve_rels_target(slide_path, target);
        let xml = match cache.get(&path) {
            Some(x) => x,
            None => continue,
        };
        let shapes = match extract_shapes(xml) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for s in shapes {
            if s.ph.as_deref() == Some("body") {
                let t = s.text.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        // 与 Python 一致：只检查第一个 notesSlide
        break;
    }
    None
}

/// 将 rels 中相对于源文件的 Target 路径解析为 ZIP 中的绝对路径。
///
/// source_path: "ppt/slides/slide1.xml"
/// target:     "../charts/chart1.xml"
/// result:     "ppt/charts/chart1.xml"
fn resolve_rels_target(source_path: &str, target: &str) -> String {
    let source_dir = source_path
        .rfind('/')
        .map(|i| &source_path[..i])
        .unwrap_or("");
    let mut result = source_dir.to_string();
    let mut remaining = target;
    while remaining.starts_with("../") {
        if let Some(idx) = result.rfind('/') {
            result = result[..idx].to_string();
        } else {
            result.clear();
        }
        remaining = &remaining[3..];
    }
    if result.is_empty() {
        remaining.to_string()
    } else {
        format!("{}/{}", result, remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = concat!(
        r#"xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" "#,
        r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
        r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" "#,
        r#"xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart""#
    );

    fn slide(body: &str) -> String {
        format!(r#"<p:sld {NS}><p:cSld><p:spTree>{body}</p:spTree></p:cSld></p:sld>"#)
    }

    #[test]
    fn test_slide_list() {
        let xml = concat!(
            r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"  <p:sldIdLst>"#,
            r#"    <p:sldId id="256" r:id="rId1"/>"#,
            r#"    <p:sldId id="257" r:id="rId2"/>"#,
            r#"  </p:sldIdLst>"#,
            r#"</p:presentation>"#
        );
        let rids = parse_slide_list(xml).unwrap();
        assert_eq!(rids, vec!["rId1", "rId2"]);
    }

    /// sldIdLst 为空 / rels 缺失时按文件名序号回退（Python `_slide_order` 的兜底）。
    #[test]
    fn slide_order_falls_back_to_filename() {
        let mut cache = HashMap::new();
        for name in [
            "ppt/slides/slide2.xml",
            "ppt/slides/slide10.xml",
            "ppt/slides/slide1.xml",
            "ppt/slideLayouts/slideLayout1.xml",
        ] {
            cache.insert(name.to_string(), String::new());
        }
        let got: Vec<usize> = slides_by_filename(&cache)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(got, vec![1, 2, 10]);
    }

    /// 形状文本：同一形状内的多个 run 拼接成一个项（Python 也是整形状一项）。
    #[test]
    fn shape_text_joins_runs() {
        let xml = slide(concat!(
            r#"<p:sp><p:spPr><a:off x="10" y="20"/></p:spPr><p:txBody>"#,
            r#"<a:p><a:r><a:t>Hello</a:t></a:r><a:r><a:t> World</a:t></a:r><a:br/>"#,
            r#"<a:r><a:t>next</a:t></a:r></a:p></p:txBody></p:sp>"#
        ));
        let shapes = extract_shapes(&xml).unwrap();
        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].text, "Hello World\nnext");
        assert_eq!(shapes[0].xy, Some((10, 20)));
    }

    /// 标题占位符 + 项目符号：分别还原为 Heading(1) 与 List。
    #[test]
    fn title_and_bullets_classified() {
        let xml = slide(concat!(
            r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>"#,
            r#"<p:spPr><a:off x="0" y="0"/></p:spPr><p:txBody><a:p><a:r><a:t>季度汇报</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"<p:sp><p:spPr><a:off x="0" y="100"/></p:spPr><p:txBody><a:p><a:pPr><a:buChar char="•"/></a:pPr>"#,
            r#"<a:r><a:t>要点一</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"<p:sp><p:spPr><a:off x="0" y="200"/></p:spPr><p:txBody><a:p><a:pPr><a:buAutoNum type="arabicPeriod"/></a:pPr>"#,
            r#"<a:r><a:t>第一步</a:t></a:r></a:p></p:txBody></p:sp>"#
        ));
        let mut shapes = extract_shapes(&xml).unwrap();
        let cache = HashMap::new();
        let rels = HashMap::new();
        let items = build_slide_items(&mut shapes, &cache, &rels, "ppt/slides/slide1.xml", None);

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].text, "季度汇报");
        assert_eq!(items[0].level, Some(1));
        assert_eq!(items[1].text, "要点一");
        assert_eq!(items[1].list, Some(false)); // buChar → 无序
        assert_eq!(items[2].text, "第一步");
        assert_eq!(items[2].list, Some(true)); // buAutoNum → 有序
    }

    /// 表格形状：行列结构进 Table 项，且不再输出平铺文本。
    #[test]
    fn table_shape_becomes_table_item() {
        let xml = slide(concat!(
            r#"<p:graphicFrame><p:xfrm><a:off x="0" y="0"/></p:xfrm><a:graphic><a:graphicData>"#,
            r#"<a:tbl><a:tr><a:tc><a:txBody><a:p><a:r><a:t>表A</a:t></a:r></a:p></a:txBody></a:tc>"#,
            r#"<a:tc><a:txBody><a:p><a:r><a:t>表B</a:t></a:r></a:p></a:txBody></a:tc></a:tr>"#,
            r#"<a:tr><a:tc><a:txBody><a:p><a:r><a:t>a1</a:t></a:r></a:p></a:txBody></a:tc>"#,
            r#"<a:tc><a:txBody><a:p><a:r><a:t>b1</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl>"#,
            r#"</a:graphicData></a:graphic></p:graphicFrame>"#
        ));
        let mut shapes = extract_shapes(&xml).unwrap();
        let items = build_slide_items(
            &mut shapes,
            &HashMap::new(),
            &HashMap::new(),
            "ppt/slides/slide1.xml",
            None,
        );

        assert_eq!(items.len(), 1);
        let rows = items[0].table.as_ref().expect("应为表格项");
        assert_eq!(
            rows.rows,
            vec![
                vec!["表A".to_string(), "表B".to_string()],
                vec!["a1".to_string(), "b1".to_string()],
            ]
        );
        assert_eq!(items[0].text, "表A,表B\na1,b1");
        assert_eq!(items[0].level, None);
    }

    /// 图表形状：记录 r:id，文本取自图表部件（标题 + 数据标签）。
    #[test]
    fn chart_shape_reads_chart_part() {
        let xml = slide(concat!(
            r#"<p:graphicFrame><p:xfrm><a:off x="0" y="0"/></p:xfrm><a:graphic><a:graphicData>"#,
            r#"<c:chart r:id="rIdChart"/></a:graphicData></a:graphic></p:graphicFrame>"#
        ));
        let mut shapes = extract_shapes(&xml).unwrap();
        assert_eq!(shapes[0].chart_rid.as_deref(), Some("rIdChart"));

        let chart_xml = concat!(
            r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" "#,
            r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<c:chart><c:title><c:tx><c:rich><a:p><a:r><a:t>系列一</a:t></a:r></a:p></c:rich></c:tx></c:title>"#,
            r#"<c:plotArea><c:dLbls><c:dLbl><c:tx><c:rich><a:p><a:r><a:t>占比12%</a:t></a:r></a:p></c:rich></c:tx></c:dLbl>"#,
            r#"<c:dLbl><c:tx><c:strRef><c:strCache><c:pt><c:v>占比34%</c:v></c:pt></c:strCache></c:strRef></c:tx></c:dLbl>"#,
            r#"</c:dLbls></c:plotArea></c:chart></c:chartSpace>"#
        );
        let mut cache = HashMap::new();
        cache.insert("ppt/charts/chart1.xml".to_string(), chart_xml.to_string());
        let mut rels = HashMap::new();
        rels.insert("rIdChart".to_string(), "../charts/chart1.xml".to_string());

        let items = build_slide_items(&mut shapes, &cache, &rels, "ppt/slides/slide1.xml", None);
        assert_eq!(items.len(), 1);
        assert!(items[0].chart);
        assert_eq!(items[0].text, "系列一\n占比12%\n占比34%");
    }

    /// 备注页：只取 body 占位符，且不进块（由 assemble 追加到页文本）。
    #[test]
    fn notes_use_body_placeholder() {
        let notes = concat!(
            r#"<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" "#,
            r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<p:cSld><p:spTree>"#,
            r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="sldNum"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>7</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>演讲者备注文本</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"</p:spTree></p:cSld></p:notes>"#
        );
        let mut cache = HashMap::new();
        cache.insert(
            "ppt/notesSlides/notesSlide1.xml".to_string(),
            notes.to_string(),
        );
        let mut rels = HashMap::new();
        rels.insert(
            "rIdNotes".to_string(),
            "../notesSlides/notesSlide1.xml".to_string(),
        );

        let text = notes_text(&cache, &rels, "ppt/slides/slide1.xml").unwrap();
        assert_eq!(text, "演讲者备注文本");
    }

    /// 几何排序：上→下、左→右；无坐标的形状排在有坐标之后。
    #[test]
    fn shapes_sorted_by_geometry() {
        let xml = slide(concat!(
            r#"<p:sp><p:spPr><a:off x="0" y="500"/></p:spPr><p:txBody><a:p><a:r><a:t>下</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"<p:sp><p:spPr><a:off x="100" y="0"/></p:spPr><p:txBody><a:p><a:r><a:t>右上</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"<p:sp><p:spPr><a:off x="0" y="0"/></p:spPr><p:txBody><a:p><a:r><a:t>左上</a:t></a:r></a:p></p:txBody></p:sp>"#
        ));
        let mut shapes = extract_shapes(&xml).unwrap();
        let items = build_slide_items(
            &mut shapes,
            &HashMap::new(),
            &HashMap::new(),
            "ppt/slides/slide1.xml",
            None,
        );
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, vec!["左上", "右上", "下"]);
    }

    /// 三页最小演示（每页一句话、第 2 页带备注），供预算 / 上限用例复用。
    fn three_slide_pptx(dir: &str, file: &str) -> std::path::PathBuf {
        use std::io::Write;

        fn text_slide(text: &str) -> String {
            slide(&format!(
                r#"<p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>"#
            ))
        }
        let pres = concat!(
            r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<p:sldIdLst><p:sldId id="256" r:id="rId1"/><p:sldId id="257" r:id="rId2"/>"#,
            r#"<p:sldId id="258" r:id="rId3"/></p:sldIdLst></p:presentation>"#
        );
        let pres_rels = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>"#,
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/>"#,
            r#"<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide3.xml"/>"#,
            r#"</Relationships>"#
        );
        let slide2_rels = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rIdNotes" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/>"#,
            r#"</Relationships>"#
        );
        let notes = concat!(
            r#"<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" "#,
            r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>"#,
            r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr>"#,
            r#"<p:txBody><a:p><a:r><a:t>备注乙</a:t></a:r></a:p></p:txBody></p:sp>"#,
            r#"</p:spTree></p:cSld></p:notes>"#
        );

        let dir = std::env::temp_dir().join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            let entries = [
                ("ppt/presentation.xml", pres.to_string()),
                ("ppt/_rels/presentation.xml.rels", pres_rels.to_string()),
                ("ppt/slides/slide1.xml", text_slide("甲乙")),
                ("ppt/slides/slide2.xml", text_slide("丙丁")),
                ("ppt/slides/slide3.xml", text_slide("戊己")),
                ("ppt/slides/_rels/slide2.xml.rels", slide2_rels.to_string()),
                ("ppt/notesSlides/notesSlide1.xml", notes.to_string()),
            ];
            for (name, body) in entries {
                zw.start_file(name, opts).unwrap();
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        path
    }

    /// 截断预算：按页累计；命中预算的那一页**整页保留**、备注不读、后续页不再解析
    /// （对齐 Python `_budget` + `if want_notes and not truncated`）。
    #[test]
    fn truncate_keeps_overflow_page_and_drops_later_slides() {
        let path = three_slide_pptx("docparser_office_pptx_truncate", "truncate.pptx");
        let p = path.to_str().unwrap().to_string();

        // 预算 = 2 字符：slide1(2) 放得下；slide2(2) 命中截断但仍整页保留；slide3 不再解析
        // （fixture 每页只有 1 个形状 → auto 会走 raw；本用例验的是 semantic 下的预算）
        let on = PptxRawBackend::default()
            .with_structure_mode(StructureMode::Semantic)
            .with_truncate(1, 2)
            .parse(&p)
            .unwrap();
        // 未截断：三页都在，slide2 有备注
        let off = PptxRawBackend::default()
            .with_structure_mode(StructureMode::Semantic)
            .parse(&p)
            .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(on.pages.len(), 2, "命中预算后不再解析后续 slide");
        assert_eq!(on.pages[1].items[0].text, "丙丁", "命中页整页保留");
        assert_eq!(on.pages[1].notes, None, "命中页不读备注");
        assert_eq!(off.pages.len(), 3);
        assert_eq!(off.pages[1].notes.as_deref(), Some("备注乙"));
    }

    /// 幻灯片上限：切片保留原页码，且**不**抑制备注（不是预算截断）
    /// —— 对齐 Python `slides = slides[:max_slides]` 后再走 `truncated` 逻辑。
    #[test]
    fn max_slides_drops_extra_slides_and_keeps_notes() {
        let path = three_slide_pptx("docparser_office_pptx_max_slides", "max.pptx");
        let p = path.to_str().unwrap().to_string();
        let doc = PptxRawBackend::default()
            .with_structure_mode(StructureMode::Semantic)
            .with_max_slides(2)
            .parse(&p)
            .unwrap();
        let all = PptxRawBackend::default()
            .with_structure_mode(StructureMode::Semantic)
            .parse(&p)
            .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(doc.pages.len(), 2, "只留前 2 页");
        assert_eq!(doc.pages[0].name.as_deref(), Some("slide-1"));
        assert_eq!(doc.pages[1].name.as_deref(), Some("slide-2"));
        assert_eq!(
            doc.pages[1].notes.as_deref(),
            Some("备注乙"),
            "上限不是预算截断，备注照读"
        );
        assert_eq!(all.pages.len(), 3, "0 = 不限");
    }

    /// raw 快路径（`structure_mode` 非语义）：**不产结构块（组装层按行产
    /// `Paragraph` 块，与 docx 快路径同口径）、不读备注**，页文本是
    /// 各形状文本按 `\n` 拼接——单形状页在 `auto` 下就会走这条路（对齐 Python
    /// `len(shapes) > 1 or tbl or chart` 的 probe）。
    #[test]
    fn raw_mode_emits_flat_text_and_drops_notes() {
        let path = three_slide_pptx("docparser_office_pptx_raw", "raw.pptx");
        let p = path.to_str().unwrap().to_string();
        // auto：单形状页 → raw
        let auto = PptxRawBackend::default().parse(&p).unwrap();
        // 显式 semantic：同一文件走全结构
        let sem = PptxRawBackend::default()
            .with_structure_mode(StructureMode::Semantic)
            .parse(&p)
            .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(auto.pages.len(), 3);
        assert!(auto.pages[0].items.is_empty(), "raw 不产项");
        assert_eq!(auto.pages[0].flat_text.as_deref(), Some("甲乙"));
        assert!(auto.pages[1].notes.is_none(), "raw 整体丢弃备注");
        assert_eq!(auto.pages[1].flat_text.as_deref(), Some("丙丁"));

        assert_eq!(sem.pages[0].items.len(), 1, "semantic 照常成块");
        assert!(sem.pages[0].flat_text.is_none());
        assert_eq!(sem.pages[1].notes.as_deref(), Some("备注乙"));

        // 组装层：raw 页也按行产 Paragraph 块（快路径不再是零块，翻译/RAG 可用）
        use crate::plugin::FormatPlugin;
        let doc = crate::backends::office::OfficePlugin::default().assemble(&auto);
        for (page, head) in doc.pages.iter().zip(["甲乙", "丙丁", "戊己"]) {
            assert_eq!(page.blocks.len(), 1);
            assert_eq!(page.blocks[0].kind, crate::model::BlockKind::Paragraph);
            assert_eq!(page.blocks[0].content, head);
            assert_eq!(page.text, head);
        }
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

    /// 图片形状（`p:pic`）：没有文本，只有一条 `a:blip` 引用。
    fn pic(rid: &str) -> String {
        format!(r#"<p:pic><p:blipFill><a:blip r:embed="{rid}"/></p:blipFill></p:pic>"#)
    }

    /// 内嵌图（`ocr_images`）：
    /// * 图片项排在**同一页前面的文本项之后**（Python 逐形状 append）；
    /// * 去重域是**单页**（`seen_media` 每页新建）：同页重复引用只出一张，
    ///   跨页同图各自出（每页各有一份）；
    /// * 过小（`min(w,h) < 50`）的装饰图不产项；
    /// * raw 页不出图（Python pptx 快路径直接 return）。
    #[test]
    fn images_are_per_page_and_follow_shape_order() {
        use std::io::Write;

        fn text_sp(text: &str) -> String {
            format!(r#"<p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>"#)
        }
        let pres = concat!(
            r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<p:sldIdLst><p:sldId id="256" r:id="rId1"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst>"#,
            r#"</p:presentation>"#
        );
        let pres_rels = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>"#,
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/>"#,
            r#"</Relationships>"#
        );
        let slide_rels = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/pic1.png"/>"#,
            r#"<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/tiny.png"/>"#,
            r#"</Relationships>"#
        );

        let slide1 = slide(&format!(
            "{}{}{}{}",
            text_sp("图前"),
            pic("rId2"),
            pic("rId2"), // 同页重复 → 去重
            pic("rId3"), // 20×20 → 过滤
        ));
        let slide2 = slide(&format!("{}{}", text_sp("图后"), pic("rId2")));

        let dir = std::env::temp_dir().join("docparser_office_pptx_images");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("images.pptx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("ppt/presentation.xml", pres.to_string()),
                ("ppt/_rels/presentation.xml.rels", pres_rels.to_string()),
                ("ppt/slides/slide1.xml", slide1),
                ("ppt/slides/slide2.xml", slide2),
                ("ppt/slides/_rels/slide1.xml.rels", slide_rels.to_string()),
                ("ppt/slides/_rels/slide2.xml.rels", slide_rels.to_string()),
            ] {
                zw.start_file(name, opts).unwrap();
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.start_file("ppt/media/pic1.png", opts).unwrap();
            zw.write_all(&png_bytes(120, 80)).unwrap();
            zw.start_file("ppt/media/tiny.png", opts).unwrap();
            zw.write_all(&png_bytes(20, 20)).unwrap();
            zw.finish().unwrap();
        }
        let p = path.to_str().unwrap().to_string();

        let on = PptxRawBackend::default()
            .with_ocr_images(true)
            .parse(&p)
            .unwrap();
        let off = PptxRawBackend::default().parse(&p).unwrap();
        let raw = PptxRawBackend::default()
            .with_structure_mode(StructureMode::Raw)
            .with_ocr_images(true)
            .parse(&p)
            .unwrap();
        std::fs::remove_file(&path).ok();

        for (page, head) in on.pages.iter().zip(["图前", "图后"]) {
            let kinds: Vec<&str> = page
                .items
                .iter()
                .map(|i| if i.image.is_some() { "image" } else { "text" })
                .collect();
            assert_eq!(kinds, ["text", "image"], "图片紧跟同页文本");
            assert_eq!(page.items[0].text, head);
            let im = page.items[1].image.as_ref().expect("每页各有一张图");
            assert_eq!(im.ext, "png");
            assert_eq!(im.px, Some((120, 80)));
            assert_eq!(im.data, png_bytes(120, 80), "同图跨页读取到同一份字节");
        }

        // 默认关
        assert!(off.pages[0].items.iter().all(|i| i.image.is_none()));
        // raw：不开图（Python pptx 快路径直接 return 文本）
        assert!(raw.pages[0].items.is_empty());
        assert_eq!(raw.pages[0].flat_text.as_deref(), Some("图前"));

        // 组装层：每页 images 一张、Image 块 content 为空
        let doc = crate::backends::office::assemble(&on, &crate::backends::office::OfficeOptions::default());
        for page in &doc.pages {
            assert_eq!(page.images.len(), 1);
            assert!(page.images[0].data.is_empty(), "字节不落 IR");
        }
        assert_eq!(doc.pages[0].blocks.len(), 2);
        assert_eq!(doc.pages[0].blocks[1].content, "");
    }
}
