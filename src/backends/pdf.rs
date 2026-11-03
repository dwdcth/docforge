// PDF 后端：页级混合路由 + OCR 引擎接入（feature = "pdf"）。
//
// 页级路由流程**参考 rumi-ocr `ocr/src/pdf_route.rs` 的设计重新实现**
//（docparse-plan §12：rumi 为 MulanPSL-2.0，仅设计参考，不复制源码）：
// pdf-inspector 逐页分类 → 文字层页直提（TextLayer 块）→ 扫描页按连续区间
// pdftoppm 渲染 + OcrEngine 识别（Ocr 块带 confidence）。
//
// # 坐标系契约
//
// pdf-inspector 的 TextItem 坐标 = **可见页框（CropBox∩MediaBox）内、左下角
// 为原点、y 向上、单位 PDF 点（1/72 英寸）**；y 对水平文本是基线（下缘），
// height 是 em 高。其公开 API 不暴露页框高度，无法在渲染图像的左上原点
// 坐标系间换算，故：
// - 文字层页 `Page.coord_units = Some(CoordUnits::Pt)`：保持 pdf-inspector
//   原坐标系（Rect 的 y0 = PDF 空间较小值 = 行的下缘）；
// - OCR 页 `Some(CoordUnits::Px { dpi })`：渲染图像坐标（左上原点），
//   dpi 即渲染分辨率，可换算回 PDF 点（pt = px / dpi * 72）。
// - 混合页（页内分治，M4 实验特性 `intra_page`）统一 `Some(CoordUnits::Pt)`：
//   文字层坐标零损耗；OCR 行 bbox 由整页渲染像素按 dpi 精确缩放 + y 翻转
//   回 PDF 点（pt = px * 72 / dpi），含 ±1px 取整误差。
// 同一文档两坐标系并存，消费者按 `Page.coord_units` 区分（归一化坐标列为
// 远期重构项，docparse-plan §14 Q1）。
//
// 已知限制：`/Rotate ≠ 0` 页的混合分治本轮不判旋转（文字行 rotation 本轮
// 不读），此类页 OCR 块坐标可能失真，后续版本补。
//
// # rumi 集成增强
//
// - `Block.quad` 四点框直通：扫描页 OCR 行 `RecognizedLine.quad` 原样进
//   RawTextItem/Block（旋转保真）；文字层路径恒 None。页内分治（M4）的
//   OCR 行也不出 quad——其四点框在裁剪图局部 px 系，直通会与混合页统一
//   Pt 的 `coord_units` 语义冲突（见 crop_and_ocr_region 注释）。
// - `PdfOptions.on_progress` 页级进度/取消：逐页生产循环每页产出即回调
//   （页序单调），false 取消——三层截停点止住后续算力（见"逐页懒式管线"），
//   已产出页照常返回，余页出空占位保持页号对齐，warnings 记中止标记
//  （Ok 语义，不报错）。
// - `PdfOptions.keep_raster_pages` 扫描页渲染图保留：与 OCR 消费同一渲染
//   图，PNG 字节进该页 RawPage.images（assemble 透传 Page.images）。
// - `parse_streaming` pdf 深度流式入口：懒式循环逐页产 Page 直接投递 sink
//   （RawDoc 不攒，峰值内存 = 单页，含 keep_raster_pages 的页图），统计按
//   已投递页回填；sink false 与 on_progress false 同一截停语义。批式与
//   流式共用同一单页生产循环体（PageProducer::produce_page）。
//
// # 逐页懒式管线（取消即截停算力）
//
// parse_pdf 不再"先全算后组装"，而是分类后按页序逐页生产、每页产出即回调。
// on_progress 返回 false 置取消标志后，三层截停点让后续算力不再发生：
// 1. 懒提取：首个文字层页到达才调批量文字提取（一次全量缓存供后续页查缓存）；
//    取消发生在提取前则整段提取不发生；
// 2. 区间粒度渲染：扫描页保持连续区间合并渲染（一次 pdftoppm -f/-l，性能位，
//    无取消损耗）；取消先于区间首页则整段渲染不发起；
// 3. 逐页 OCR 检查点：区间渲染是子进程一次性出全区间、无法逐页拆分，但区间
//    内每页 OCR（算力大头）前查取消，取消即止；keep_raster_pages 的 PNG
//    编码随之跳过。
// 已产出页保留，余页空占位（页号对齐），warnings 记中止页（Ok 语义）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use pdf_inspector::extractor::{group_into_lines_preserving_all_text, ItemType};
use pdf_inspector::{DetectionConfig, ScanStrategy};

use crate::error::{DocError, ErrorCode, Result};
use crate::model::{blocks_to_text, Block, BlockKind, BlockSource, CoordUnits, DocMeta, Document,
    Page, Rect};
use crate::ocr::OcrEngine;
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawImage, RawPage, RawTextItem};
use crate::registry::Registry;

/// 默认渲染 DPI（A4 @200dpi ≈ 1654×2339，与 rumi 实践一致）。
pub const DEFAULT_DPI: u32 = 200;

/// PDF 页级路由模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OcrMode {
    /// 逐页判定：文字层页直提，扫描页渲染 + OCR（默认）。
    #[default]
    Auto,
    /// 全部页渲染 + OCR（强制慢路径，行为等价 rumi `--force-raster`）。
    On,
    /// 仅文字层；扫描页产空页并记 warnings（不报错）。
    Off,
}

/// PDF 后端选项。
#[derive(Clone)]
pub struct PdfOptions {
    pub mode: OcrMode,
    /// 扫描页渲染 DPI（`CoordUnits::Px` 的 dpi 随此值）。
    pub dpi: u32,
    /// OCR 引擎（`Auto`/`On` 且存在扫描页时必需；`Off` 不用）。
    pub engine: Option<Arc<dyn OcrEngine>>,
    /// M4 页内混合路由（实验特性）：文字层页内未被文字覆盖的大图区做
    /// 整页渲染 + 分区域 OCR。仅 `OcrMode::Auto` 且存在引擎时生效；
    /// 默认 false，行为与现状（纯页级路由）完全等价。
    pub intra_page: bool,
    /// 页级进度回调：每页处理完成时回调 (已处理页数, 总页数)；返回 false
    /// 取消解析——此后不再发起文字层提取、区间渲染与 OCR（已开始的当前
    /// 区间渲染不中断，区间内余页 OCR 逐页截停）；已产出页保留，余页出空
    /// 占位保持页号对齐，warnings 附"用户中止于第 N 页"（Ok 语义，不报错）。
    /// 页序单调；全同步，实现方不得在回调里再调本引擎。
    pub on_progress: Option<std::sync::Arc<dyn Fn(u32, u32) -> bool + Send + Sync>>,
    /// 扫描页渲染图保留：true 时渲染页的 PNG 字节进该页 Page.images
    ///（Image.data，ext="png"）——原文/译文对照预览类需求。默认 false。
    /// 注意 IR 的 JSON 序列化会 base64 编码图片字节，输出体积随之增大。
    pub keep_raster_pages: bool,
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self {
            mode: OcrMode::Auto,
            dpi: DEFAULT_DPI,
            engine: None,
            intra_page: false,
            on_progress: None,
            keep_raster_pages: false,
        }
    }
}

/// 注册 pdf 后端（解析能力 + 文档插件）到 `registry`。
pub fn register(registry: &mut Registry, opts: PdfOptions) {
    use std::sync::Arc as StdArc;
    let cap: StdArc<dyn ParseCapability> = StdArc::new(PdfParseCapability { opts });
    if let Err(e) = registry.register_parse(cap, None) {
        panic!("pdf 后端注册失败：{e}");
    }
    registry.register_plugin(StdArc::new(PdfPlugin));
}

// ---------------------------------------------------------------------------
// 解析能力：文件 → RawDoc（页级路由全流程）
// ---------------------------------------------------------------------------

struct PdfParseCapability {
    opts: PdfOptions,
}

impl ParseCapability for PdfParseCapability {
    fn id(&self) -> &str {
        "pdf-inspector"
    }

    fn formats(&self) -> &[&str] {
        &["pdf"]
    }

    fn parse(&self, path: &Path) -> Result<RawDoc> {
        parse_pdf(path, &self.opts)
    }
}

/// 页级路由主流程（逐页懒式管线，见模块头"逐页懒式管线"节）。
/// 全量批式：逐页生产攒 RawDoc 返回（深度流式变体见 [`parse_streaming`]）。
fn parse_pdf(path: &Path, opts: &PdfOptions) -> Result<RawDoc> {
    let mut producer = PageProducer::new(path, opts)?;
    let mut pages: Vec<RawPage> = Vec::with_capacity(producer.page_count as usize);
    for p in 1..=producer.page_count {
        // 占位页（取消后）不回调——回调只发生在真实页产出点（与旧实现一致）
        let already_canceled = producer.is_canceled();
        let page = producer.produce_page(p)?;
        pages.push(page);
        if !already_canceled {
            producer.report(p);
        }
    }
    producer.push_abort_warning();
    let needs_ocr = producer.needs_ocr();
    let mut meta = std::collections::HashMap::new();
    if let Some(w) = producer.warnings.pop() {
        meta.insert("warnings".to_string(), w);
    }
    Ok(RawDoc { pages, meta, needs_ocr })
}

/// pdf 深度流式：懒式管线的逐页循环直接产 Page 投递 sink（RawDoc 不攒），
/// 峰值内存 = 单页（含 keep_raster_pages 的页图）。与 engine 层通用流式
/// [`crate::engine::DocumentEngine::parse_streaming`] 的差异：绕过 RawDoc /
/// assemble 全量驻留，页产即吐——千页扫描件 + keep_raster_pages 全量驻留
/// 爆内存的最坏场景由本入口根治。rumi 等消费者调用指引：pdf 用本深度版
///（engine 层通用版对 pdf 同样正确，只是省不掉 RawDoc 阶段），其余格式用
/// engine 层通用版。
///
/// # 语义
/// - 逐页 produce → sink 投递 → on_progress 回调（页级回调与 sink 投递同
///   点，页序单调）→ 取消检查。sink 返回 false 与 on_progress 返回 false
///   同一截停语义：余页不产不投（三层截停点全部生效：懒提取 / 区间渲染 /
///   逐页 OCR），warnings 记"用户中止于第 N 页"（与批式取消同文案，
///   Ok 语义不报错）。
/// - DocMeta 统计按已投递页回填（复用 [`Document::finalize`]；被拒页已送达
///   sink，计为已投递）。统计幽灵只保留 finalize 读取的字段（text /
///   images 条数 / ocr_text），PNG 字节随投递释放。
/// - 预检与批式一致：文件缺失 / 扫描页缺引擎的 Err 先于任何页产出；取消
///   后余页不出占位（流式无 Document 可对齐，直接停止投递）。
pub fn parse_streaming(
    path: &std::path::Path,
    opts: &PdfOptions,
    sink: &mut dyn FnMut(Page) -> bool,
) -> Result<DocMeta> {
    let mut producer = PageProducer::new(path, opts)?;
    let mut ghosts: Vec<Page> = Vec::new();
    for p in 1..=producer.page_count {
        if producer.is_canceled() {
            break; // 流式：取消后余页不产不投（占位页无消费方）
        }
        let raw_page = producer.produce_page(p)?;
        // 单页组装（行块透传 / 图片搬运与批式完全一致）→ 统计幽灵 → 投递
        let page = assemble_single(raw_page);
        ghosts.push(crate::engine::ghost_page(&page));
        if !sink(page) {
            producer.cancel_at(p);
            break;
        }
        // 页级进度回调（与 sink 投递同点）；false 已置取消，循环头停止
        if !producer.report(p) {
            break;
        }
    }
    producer.push_abort_warning();

    let mut meta = DocMeta { file_type: "pdf".to_string(), ..Default::default() };
    if let Some(w) = producer.warnings.pop() {
        meta.extra.insert("warnings".to_string(), w);
    }
    Ok(Document { meta, pages: ghosts }.finalize().meta)
}

/// 单页组装（PdfPlugin::assemble 的单页切片）：流式逐页 RawPage → Page，
/// 行块透传 / OCR 语义 / 图片搬运语义与批式完全一致。
fn assemble_single(rp: RawPage) -> Page {
    let mut doc = PdfPlugin.assemble(&RawDoc { pages: vec![rp], ..Default::default() });
    doc.pages.pop().expect("单页组装必出一页")
}

/// 一次区间渲染的在途产物（`pdftoppm -f F -l L` 一次出全区间，区间内逐页
/// 消费）。
struct PendingInterval {
    /// 下一个待消费的页号（消费点对不上时防御性重渲染新区间；正常流程恒
    /// 与扫描页号同步推进）。
    next_page: u32,
    /// 渲染产物按页序，消费完即弃（流式峰值内存的关键：区间图不跨区间驻留）。
    images: std::vec::IntoIter<image::RgbaImage>,
}

/// 逐页生产上下文：懒式管线的可变状态（懒提取缓存 / 区间渲染产物 / 取消
/// 标志 / 告警）。[`parse_pdf`]（全量批式）与 [`parse_streaming`]（深度
/// 流式）共用同一循环体——单页生产在 [`PageProducer::produce_page`]，
/// on_progress 回调点由驱动方决定（批式 = 页产出即回调；流式 = sink 投递
/// 后回调）。
struct PageProducer<'a> {
    path: &'a Path,
    opts: &'a PdfOptions,
    page_count: u32,
    /// 实际走渲染+OCR 的页（Off 模式恒空）。
    raster_pages: HashSet<u32>,
    /// 分类器判定"需要 OCR"的页（Off 告警与 needs_ocr 标记用）。
    scan_detected: Vec<u32>,
    /// 文字层页集合（懒提取的范围参数）。
    text_pages: HashSet<u32>,
    /// 懒提取缓存：首个文字层页到达才一次性全量提取并缓存（取消发生在
    /// 提取前则整段提取算力不发生）。
    text_cache: Option<(Vec<Vec<RawTextItem>>, Vec<pdf_inspector::TextItem>)>,
    /// 区间渲染在途产物（扫描区间首页到达时整段渲染，区间内逐页消费）。
    pending: Option<PendingInterval>,
    /// 取消标志：on_progress 返回 false 或流式 sink 返回 false 即置位；
    /// 各截停点只读。
    canceled: bool,
    /// 中止页（最后一条进度回调 / sink 拒绝的页号），warnings 用。
    cancelled_at: Option<u32>,
    /// 告警缓冲（Off 模式扫描页 / 页内分治降级 / 中止标记）。
    warnings: Vec<String>,
}

impl<'a> PageProducer<'a> {
    /// 预检 + 分类 + 前置告警：文件存在（Unsupported 终止码，不进解析
    /// 循环）、页级分类、Off 模式扫描页告警入列、扫描页引擎预检。批式与
    /// 流式共用——Err 一律先于任何页产出 / 回调（与旧实现一致）。
    fn new(path: &'a Path, opts: &'a PdfOptions) -> Result<Self> {
        // 预检走终止码 Unsupported：文件不存在是确定性结论，不能被兜底后端掩盖
        if !path.is_file() {
            return Err(DocError {
                code: ErrorCode::Unsupported,
                message: format!("文件不存在：{}", path.display()),
                attempts: vec![],
            });
        }

        let Classification { page_count, raster: raster_pages, scan_detected } =
            classify(path, opts.mode)?;
        let mut warnings = Vec::new();

        // Off 模式：探测出的扫描页不渲染不报错，记告警（页不出块）。先于生产
        // 循环入列（meta 只保留末条告警，取消标记最后入列优先，与旧实现一致）。
        if opts.mode == OcrMode::Off && !scan_detected.is_empty() {
            warnings.push(format!(
                "扫描页未 OCR（mode=Off）：第 {} 页",
                scan_detected
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join("、")
            ));
        }

        // 扫描页需 OCR 引擎：预检保持 Err 先于任何页产出/回调（与旧实现一致）
        if !raster_pages.is_empty() {
            opts.engine.as_ref().ok_or_else(|| DocError {
                code: ErrorCode::Unsupported,
                message: format!(
                    "第 {} 页需 OCR 引擎：开启 ocr-oar feature 或在 PdfOptions.engine 注入 OcrEngine",
                    raster_pages.iter().min().unwrap()
                ),
                attempts: vec![],
            })?;
        }

        // 文字层页集合（Off 模式 raster 集为空 → 全部页走文字层）
        let text_pages: HashSet<u32> =
            (1..=page_count).filter(|p| !raster_pages.contains(p)).collect();
        Ok(Self {
            path,
            opts,
            page_count,
            raster_pages,
            scan_detected,
            text_pages,
            text_cache: None,
            pending: None,
            canceled: false,
            cancelled_at: None,
            warnings,
        })
    }

    fn is_canceled(&self) -> bool {
        self.canceled
    }

    /// 进度回调统一包装：页序单调，false 置取消并记中止页（触发回调的
    /// 当前页已产出）。
    fn report(&mut self, done: u32) -> bool {
        let Some(cb) = self.opts.on_progress.as_ref() else { return true };
        if cb(done, self.page_count) {
            true
        } else {
            self.canceled = true;
            self.cancelled_at = Some(done);
            false
        }
    }

    /// 流式 sink 拒绝续投：与 on_progress false 同一截停语义（中止页 = 该页）。
    fn cancel_at(&mut self, done: u32) {
        self.canceled = true;
        self.cancelled_at = Some(done);
    }

    /// 中止告警（生产循环结束后调用；最后入列，meta 只保留末条告警时优先）。
    fn push_abort_warning(&mut self) {
        if let Some(done) = self.cancelled_at {
            self.warnings
                .push(format!("用户中止于第 {done} 页（共 {} 页）", self.page_count));
        }
    }

    /// 分类结论派生标记：存在"需要 OCR 却未 OCR"的页。
    fn needs_ocr(&self) -> bool {
        !self.raster_pages.is_empty()
            || (self.opts.mode == OcrMode::Off && !self.scan_detected.is_empty())
    }

    /// 单页生产（原 parse_pdf 循环体单页化）。取消后余页出空占位（页号
    /// 对齐）；扫描页到达时若无在途区间产物则发起区间渲染（一次 pdftoppm），
    /// 区间内逐页消费渲染产物进 OCR。不回调 on_progress——回调点由驱动方
    /// 决定（批式：产出即回调；流式：sink 投递后回调）。
    fn produce_page(&mut self, p: u32) -> Result<RawPage> {
        // 截停点（页级）：取消后余页全部空占位——不再回调、不再提取、
        // 不再发起区间渲染（占位保持页号对齐）
        if self.canceled {
            return Ok(RawPage { name: Some(p.to_string()), ..Default::default() });
        }
        let path = self.path;
        let opts: &PdfOptions = self.opts;

        if self.raster_pages.contains(&p) {
            // 扫描页：连续区间合并渲染（一次 pdftoppm -f/-l，性能位，无取消
            // 损耗）。取消只能经由驱动方回调 / sink 置位，本函数入口未取消
            // 才进入本分支——"区间渲染前查取消"由此覆盖：取消页起的区间
            // 整段不发起渲染。
            if self.pending.as_ref().map_or(true, |pi| pi.next_page != p) {
                let mut last = p;
                while last < self.page_count && self.raster_pages.contains(&(last + 1)) {
                    last += 1;
                }
                let images = render_pages(path, p, last, opts.dpi)?;
                self.pending = Some(PendingInterval { next_page: p, images: images.into_iter() });
            }
            // 截停点（逐页 OCR）：OCR 是算力大头，取消后余页不再进引擎，
            // keep_raster_pages 的 PNG 编码随之跳过（余页占位由驱动方逐页
            // 补出——批式；流式直接停止投递）
            let engine = opts.engine.as_ref().expect("扫描页引擎已预检");
            let (img, interval_done) = {
                let pi = self.pending.as_mut().expect("区间渲染产物在途");
                let Some(img) = pi.images.next() else {
                    return Err(DocError {
                        code: ErrorCode::Corrupt,
                        message: format!("区间渲染产物与页数不齐（第 {p} 页）"),
                        attempts: vec![],
                    });
                };
                pi.next_page += 1;
                (img, pi.images.as_slice().is_empty())
            };
            if interval_done {
                self.pending = None; // 区间产物消费完即弃（不跨区间驻留）
            }
            let lines = engine
                .recognize(&img)
                .map_err(|e| DocError { code: ErrorCode::Unknown, message: e, attempts: vec![] })?;
            let items = lines.into_iter().map(|l| {
                let bbox = l.axis_rect();
                RawTextItem {
                    text: l.text,
                    bbox: Some(bbox),
                    quad: Some(l.quad),
                    source: BlockSource::Ocr,
                    confidence: Some(l.score),
                    ..Default::default()
                }
            }).collect();
            // keep_raster_pages：与 OCR 消费同一渲染图，编码 PNG 挂页
            let page_image = if opts.keep_raster_pages {
                Some(encode_page_png(&img)?)
            } else {
                None
            };
            return Ok(RawPage {
                name: Some(p.to_string()),
                items,
                images: page_image.into_iter().collect(),
                coord_units: Some(CoordUnits::Px { dpi: opts.dpi as f32 }),
                ..Default::default()
            });
        }

        // ---- 文字层页：懒提取 → 页内分治 → 组装 ----
        if self.text_cache.is_none() {
            self.text_cache = Some(extract_text_layer(path, &self.text_pages, self.page_count)?);
        }
        let (lines_by_page, img_items) = self.text_cache.as_mut().unwrap();
        let idx = p as usize - 1;
        let mut items = std::mem::take(&mut lines_by_page[idx]);
        let mut dims: Option<(f32, f32)> = None;
        // M4 页内分治（实验特性 intra_page，仅 Auto 模式）：本页未被文字覆盖
        // 的大图区做整页渲染 + 分区域 OCR，OCR 行与文字层行按阅读序并回同一
        // 页（coord_units 统一 Pt）。有引擎才执行；无引擎时判定为真也不失败，
        // 仅记告警。/Rotate≠0 页本轮不判旋转（模块头契约已注明）。
        if opts.intra_page && opts.mode == OcrMode::Auto {
            let page_imgs: Vec<pdf_inspector::TextItem> =
                img_items.iter().filter(|it| it.page == p).cloned().collect();
            match opts.engine.as_ref() {
                // 无引擎降级：分治判定为真也不报错（任务书 §6）
                None => {
                    if needs_intra_page_split(&items, &page_imgs) {
                        self.warnings.push(format!("页 {p} 检测到未覆盖大图区，未 OCR（无引擎）"));
                    }
                }
                Some(engine) => {
                    let targets = intra_ocr_targets(&items, &page_imgs);
                    if !targets.is_empty() {
                        // 整页渲染单页（复用页级渲染；失败同页级路由一致，
                        // 致命；无产物则本页维持纯文字层组装）
                        let page_img = render_pages(path, p, p, opts.dpi)?.into_iter().next();
                        if let Some(page_img) = page_img {
                            // 由渲染图尺寸反推页面 pt 尺寸（pdf-inspector 公开
                            // API 不暴露页框大小；混合页 items 坐标统一 Pt，
                            // 顺手填 RawPage.width/height）
                            let h_pt = page_img.height() as f32 * 72.0 / opts.dpi as f32;
                            let w_pt = page_img.width() as f32 * 72.0 / opts.dpi as f32;
                            let mut ocr_items = Vec::new();
                            for t in &targets {
                                let rect =
                                    Rect { x0: t.x, y0: t.y, x1: t.x + t.width, y1: t.y + t.height };
                                match crop_and_ocr_region(&page_img, rect, opts.dpi, h_pt, engine.as_ref()) {
                                    Ok(mut its) => ocr_items.append(&mut its),
                                    Err(e) => self.warnings.push(format!("页 {p} 大图区 OCR 失败：{e}")),
                                }
                            }
                            items = merge_by_reading_order(items, ocr_items);
                            dims = Some((w_pt, h_pt));
                        }
                    }
                }
            }
        }
        Ok(RawPage {
            name: Some(p.to_string()),
            items,
            coord_units: Some(CoordUnits::Pt),
            // 混合分治页：整页渲染反推的页面 pt 尺寸（其余页维持 None）
            width: dims.map(|d| d.0),
            height: dims.map(|d| d.1),
            ..Default::default()
        })
    }
}

/// 文字层懒提取：一次性提取全部文字页（pdf-inspector 批量 API，一次调用
/// 全量缓存），行分组后按页分箱；Image 占位（pdf-inspector 已给真实页框
/// 几何：PDF pt、左下原点，y = 图框下缘）单独收集，供 M4 页内分治判定与
/// 区域裁剪；其余进常规行分组（text_line_to_item 里的 Image 过滤保留作
/// 防御）。返回 (每页 TextLayer 行 items, 全部 Image 占位)。
///
/// 提取语义与旧"先全算"路径完全一致——懒式只挪动调用时机，不改提取行为。
fn extract_text_layer(
    path: &Path,
    text_pages: &HashSet<u32>,
    page_count: u32,
) -> Result<(Vec<Vec<RawTextItem>>, Vec<pdf_inspector::TextItem>)> {
    let mut lines_by_page: Vec<Vec<RawTextItem>> = vec![Vec::new(); page_count as usize];
    if text_pages.is_empty() {
        return Ok((lines_by_page, Vec::new()));
    }
    let items_by_page = pdf_inspector::extract_text_with_positions_pages(path, Some(text_pages))
        .map_err(|e| DocError {
            code: ErrorCode::Corrupt,
            message: format!("文字层提取失败（{}）：{e}", path.display()),
            attempts: vec![],
        })?;
    let mut img_items: Vec<pdf_inspector::TextItem> = Vec::new();
    let mut plain_items: Vec<pdf_inspector::TextItem> = Vec::new();
    for it in items_by_page {
        if matches!(it.item_type, ItemType::Image) {
            img_items.push(it);
        } else {
            plain_items.push(it);
        }
    }
    for line in group_into_lines_preserving_all_text(plain_items) {
        if let Some(item) = text_line_to_item(&line) {
            lines_by_page[line.page as usize - 1].push(item);
        }
    }
    Ok((lines_by_page, img_items))
}

/// 页面分类结论：总页数 / 需渲染页 / 探测出的扫描页（Off 模式仅用于告警）。
struct Classification {
    page_count: u32,
    /// 实际走渲染+OCR 的页（Off 模式恒空）。
    raster: HashSet<u32>,
    /// 分类器判定"需要 OCR"的页（告警与 needs_ocr 标记用）。
    scan_detected: Vec<u32>,
}

/// 页面分类：一次 `ScanStrategy::Full` 逐页扫描，按模式推导渲染集合。
fn classify(path: &Path, mode: OcrMode) -> Result<Classification> {
    // Full 逐页扫描：Sample(8) 对"图封面+文字正文"型文档会误判，且页级
    // 路由需要全量 per-page 结论（rumi 实践同此）
    let info = pdf_inspector::detect_pdf_type_with_config(
        path,
        DetectionConfig { strategy: ScanStrategy::Full, ..Default::default() },
    )
    .map_err(|e| DocError {
        code: ErrorCode::Corrupt,
        message: format!("PDF 分类失败（{}）：{e}", path.display()),
        attempts: vec![],
    })?;
    let page_count = info.page_count;
    let scan_detected: Vec<u32> = info.pages_needing_ocr.clone();
    let raster = match mode {
        OcrMode::On => (1..=page_count).collect(),
        OcrMode::Auto => scan_detected.iter().copied().collect(),
        OcrMode::Off => HashSet::new(),
    };
    Ok(Classification { page_count, raster, scan_detected })
}

/// 文字层行 → RawTextItem（TextLayer）。
///
/// bbox = 成员 item 框（x 左缘 / y 底缘 / 宽 / em 高）的并集；只有文本非空
/// 且非图像占位的 item 参与并集。空文本行丢弃。坐标保持 pdf-inspector 原系
/// （PDF 点、左下原点），confidence 恒 None（文字层零识别，1.0 由消费者按需补）。
fn text_line_to_item(line: &pdf_inspector::extractor::TextLine) -> Option<RawTextItem> {
    let text = line.text();
    if text.trim().is_empty() {
        return None;
    }
    let mut x0 = f32::MAX;
    let mut x1 = f32::NEG_INFINITY;
    let mut y0 = f32::MAX;
    let mut y1 = f32::NEG_INFINITY;
    for it in &line.items {
        if it.text.trim().is_empty() || matches!(it.item_type, ItemType::Image) {
            continue;
        }
        x0 = x0.min(it.x);
        x1 = x1.max(it.x + it.width);
        y0 = y0.min(it.y);
        y1 = y1.max(it.y + it.height);
    }
    if x0 > x1 {
        return None;
    }
    Some(RawTextItem {
        text,
        bbox: Some(Rect { x0, y0, x1, y1 }),
        source: BlockSource::TextLayer,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// M4 页内混合路由：文字层页大图区判定 + 区域裁剪 OCR + 阅读序合并
// ---------------------------------------------------------------------------

/// 大图判定下限：面积 ≥ 96_000 pt²（≈ 0.2 × A4 页面 595×842）。
const INTRA_BIG_IMAGE_MIN_AREA: f32 = 96_000.0;
/// 大图判定下限：高 ≥ 60 pt（排除横幅/装饰条）。
const INTRA_BIG_IMAGE_MIN_HEIGHT: f32 = 60.0;
/// 文字行与图重叠超图面积此比例即视为"文字层已覆盖"，不再对该图 OCR（防双份）。
const INTRA_TEXT_COVER_RATIO: f32 = 0.3;

/// 页内分治判定：文字层页里是否存在"未被文字覆盖的大图区"需要区域 OCR。
///
/// 设计依据：文字行 < 3 的页直接判否——文字太少的页本就该由页级路由抓去
/// 整页 OCR，页内分治不越权（防重复路径）。
fn needs_intra_page_split(
    text_items: &[RawTextItem],
    img_items: &[pdf_inspector::TextItem],
) -> bool {
    !intra_ocr_targets(text_items, img_items).is_empty()
}

/// 分治目标集：大图占位中**没有**任一文字行 bbox 与图 bbox 重叠超图面积
/// [`INTRA_TEXT_COVER_RATIO`] 的那些（`needs_intra_page_split` 的数据面，
/// parse_pdf 复用同一份目标做裁剪，保证判定与执行一致）。
fn intra_ocr_targets<'a>(
    text_items: &[RawTextItem],
    img_items: &'a [pdf_inspector::TextItem],
) -> Vec<&'a pdf_inspector::TextItem> {
    if text_items.len() < 3 {
        return Vec::new();
    }
    let mut targets = Vec::new();
    for img in img_items.iter().filter(|it| {
        it.width * it.height >= INTRA_BIG_IMAGE_MIN_AREA
            && it.height >= INTRA_BIG_IMAGE_MIN_HEIGHT
    }) {
        let img_rect = Rect { x0: img.x, y0: img.y, x1: img.x + img.width, y1: img.y + img.height };
        let img_area = img.width * img.height;
        let covered = text_items.iter().any(|line| match line.bbox {
            Some(b) => bbox_overlap_area(&b, &img_rect) > img_area * INTRA_TEXT_COVER_RATIO,
            None => false,
        });
        if !covered {
            targets.push(img);
        }
    }
    targets
}

/// 两轴对齐矩形重叠面积（不相交为 0；坐标语义由调用方保证同系）。
fn bbox_overlap_area(a: &Rect, b: &Rect) -> f32 {
    let w = (a.x1.min(b.x1) - a.x0.max(b.x0)).max(0.0);
    let h = (a.y1.min(b.y1) - a.y0.max(b.y0)).max(0.0);
    w * h
}

/// 页内分治：整页渲染图上裁出 pt 矩形区域交 OCR，识别行坐标换算回整页 PDF 点。
///
/// 设计依据：混合页统一 Pt 坐标（文字层零损耗），OCR 行由渲染像素精确缩放
/// 翻转（x_pt = px·72/dpi；y_pt = h_pt − px·72/dpi，±1px 取整误差）；裁剪区
/// 两边 < 8px 视为无内容返回空；引擎错误原样上抛（调用方降级为告警）。
fn crop_and_ocr_region(
    page_img: &image::RgbaImage,
    rect_pt: Rect,
    dpi: u32,
    h_pt: f32,
    engine: &dyn OcrEngine,
) -> std::result::Result<Vec<RawTextItem>, String> {
    let to_px = dpi as f32 / 72.0;
    // pt（左下原点）→ px（左上原点）：rect 上缘 = h_pt − y1_pt
    let x_px = rect_pt.x0 * to_px;
    let y_px = (h_pt - rect_pt.y1) * to_px;
    let w_px = (rect_pt.x1 - rect_pt.x0) * to_px;
    let h_px = (rect_pt.y1 - rect_pt.y0) * to_px;
    // clamp 到整页图边界；取整后 <8px 的碎片不进引擎（装饰噪声）
    let (iw, ih) = (page_img.width(), page_img.height());
    let ix0 = (x_px.floor().max(0.0) as u32).min(iw);
    let iy0 = (y_px.floor().max(0.0) as u32).min(ih);
    let ix1 = ((x_px + w_px).ceil().max(0.0) as u32).min(iw);
    let iy1 = ((y_px + h_px).ceil().max(0.0) as u32).min(ih);
    if ix1 <= ix0 || iy1 <= iy0 || ix1 - ix0 < 8 || iy1 - iy0 < 8 {
        return Ok(Vec::new());
    }
    let crop = image::imageops::crop_imm(page_img, ix0, iy0, ix1 - ix0, iy1 - iy0).to_image();
    let lines = engine.recognize(&crop)?;
    let to_pt = 72.0 / dpi as f32;
    Ok(lines
        .into_iter()
        .map(|l| {
            let r = l.axis_rect(); // 裁剪图局部 px（左上原点）
            // 局部 px + 裁剪偏移 → 整页 px → 整页 pt（y 翻转：像素上缘 = PDF 大 y）
            let px_x0 = ix0 as f32 + r.x0;
            let px_x1 = ix0 as f32 + r.x1;
            let px_top = iy0 as f32 + r.y0;
            let px_bottom = iy0 as f32 + r.y1;
            // quad 恒 None（缺省）：l.quad 在裁剪图局部 px 系（左上原点），
            // 直通会与混合页统一 Pt 的 Page.coord_units 语义冲突；旋转保真
            // 四点框只在页级路由的扫描页直通（页坐标系即渲染图坐标系）。
            RawTextItem {
                text: l.text,
                bbox: Some(Rect {
                    x0: px_x0 * to_pt,
                    y0: h_pt - px_bottom * to_pt, // 像素下缘 → PDF 小 y（下缘）
                    x1: px_x1 * to_pt,
                    y1: h_pt - px_top * to_pt, // 像素上缘 → PDF 大 y（上缘）
                }),
                source: BlockSource::Ocr,
                confidence: Some(l.score),
                ..Default::default()
            }
        })
        .collect())
}

/// 混合页阅读序合并：文字层 items 与 OCR items 按 bbox.y1 降序（PDF 左下
/// 原点，y1 大 = 页面上方）、同 y 按 x0 升序；无 bbox 的项排末尾（防御：
/// 正常流程两种来源都带 bbox）。设计依据：混合页统一 Pt 后两来源可直接排序。
fn merge_by_reading_order(mut a: Vec<RawTextItem>, mut b: Vec<RawTextItem>) -> Vec<RawTextItem> {
    a.append(&mut b);
    a.sort_by(|x, y| match (x.bbox, y.bbox) {
        (Some(bx), Some(by)) => by
            .y1
            .partial_cmp(&bx.y1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| bx.x0.partial_cmp(&by.x0).unwrap_or(std::cmp::Ordering::Equal)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    a
}

/// `pdftoppm -png -r {dpi} -f {first} -l {last}` 渲染闭区间页（1 起始）。
///
/// 系统依赖 poppler-utils；临时目录用后即删。返回页序与区间一致。
fn render_pages(
    path: &Path,
    first: u32,
    last: u32,
    dpi: u32,
) -> Result<Vec<image::RgbaImage>> {
    let dir = std::env::temp_dir().join(format!(
        "docforge-pdf-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).map_err(io_to_docerror)?;
    let result = render_via_pdftoppm(path, first, last, dpi, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn render_via_pdftoppm(
    path: &Path,
    first: u32,
    last: u32,
    dpi: u32,
    dir: &Path,
) -> Result<Vec<image::RgbaImage>> {
    let prefix = dir.join("page");
    let out = Command::new("pdftoppm")
        .arg("-png")
        .arg("-r")
        .arg(dpi.to_string())
        .arg("-f")
        .arg(first.to_string())
        .arg("-l")
        .arg(last.to_string())
        .arg(path)
        .arg(&prefix)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => DocError {
                code: ErrorCode::Unknown,
                message: "找不到 pdftoppm（需安装 poppler-utils）".to_string(),
                attempts: vec![],
            },
            _ => io_to_docerror(e),
        })?;
    if !out.status.success() {
        return Err(DocError {
            code: ErrorCode::Corrupt,
            message: format!("pdftoppm 失败：{}", String::from_utf8_lossy(&out.stderr).trim()),
            attempts: vec![],
        });
    }
    // pdftoppm 输出名 page-N.png（总页数 ≥10 时补零 page-01.png）：
    // 收集目录内产物，按解析出的页码数字排序保证页序
    let mut numbered: Vec<(u32, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(io_to_docerror)? {
        let entry = entry.map_err(io_to_docerror)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(no) = name
            .strip_prefix("page-")
            .and_then(|s| s.strip_suffix(".png"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        numbered.push((no, entry.path()));
    }
    numbered.sort_by_key(|(no, _)| *no);
    numbered
        .into_iter()
        .map(|(_, p)| {
            image::open(&p)
                .map(|img| img.to_rgba8())
                .map_err(|e| DocError {
                    code: ErrorCode::Corrupt,
                    message: format!("渲染图装载失败（{}）：{e}", p.display()),
                    attempts: vec![],
                })
        })
        .collect()
}

fn io_to_docerror(e: std::io::Error) -> DocError {
    DocError { code: ErrorCode::Unknown, message: format!("I/O 错误：{e}"), attempts: vec![] }
}

/// 渲染页图 → PNG 字节（`keep_raster_pages` 用：与 OCR 消费**同一渲染图**，
/// 不二次渲染）。`RawImage.px` 记录渲染像素尺寸，供消费者换算/校验。
fn encode_page_png(img: &image::RgbaImage) -> Result<RawImage> {
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder};
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(img.as_raw(), img.width(), img.height(), ExtendedColorType::Rgba8)
        .map_err(|e| DocError {
            code: ErrorCode::Unknown,
            message: format!("渲染页 PNG 编码失败：{e}"),
            attempts: vec![],
        })?;
    Ok(RawImage {
        data: png,
        ext: "png".to_string(),
        px: Some((img.width(), img.height())),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// 文档插件：RawDoc → IR（行块透传 OCR 语义字段）
// ---------------------------------------------------------------------------

struct PdfPlugin;

impl FormatPlugin for PdfPlugin {
    fn id(&self) -> &str {
        "pdf"
    }

    fn formats(&self) -> &[&str] {
        &["pdf"]
    }

    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["pdf-inspector"]
    }

    fn default_process_chain(&self) -> Vec<&'static str> {
        vec![]
    }

    fn assemble(&self, raw: &RawDoc) -> Document {
        let pages = raw
            .pages
            .iter()
            .map(|rp| {
                let blocks: Vec<Block> = rp
                    .items
                    .iter()
                    .filter(|i| !i.text.trim().is_empty())
                    .map(|i| Block {
                        kind: BlockKind::Paragraph,
                        content: i.text.clone(),
                        bbox: i.bbox,
                        quad: i.quad,
                        confidence: i.confidence,
                        source: i.source,
                        ..Default::default()
                    })
                    .collect();
                let text = blocks_to_text(&blocks, "\n");
                // RawImage（plugin 层）→ Image（IR 层）：字节/框/图内 OCR 文本
                // 保留（keep_raster_pages 的渲染页图由此进 Page.images）
                let images: Vec<crate::model::Image> = rp
                    .images
                    .iter()
                    .map(|im| crate::model::Image {
                        data: im.data.clone(),
                        bbox: im.bbox,
                        ocr_text: im.ocr_text.clone(),
                        ..Default::default()
                    })
                    .collect();
                Page {
                    name: rp.name.clone(),
                    blocks,
                    text,
                    images,
                    coord_units: rp.coord_units,
                    ..Default::default()
                }
            })
            .collect();
        Document { pages, ..Default::default() }
    }
}

// ---------------------------------------------------------------------------
// 测试样例生成（金测件构造，策略参考 rumi pdf_route::sample：系统 CJK 字体
// 探测 + ttc 抽面 + 缺字形检查 + oxidize-pdf 写侧生成）
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod sample {
    use oxidize_pdf::text::Font;
    use oxidize_pdf::{Document, Image, Page};
    use std::path::Path;

    /// 样例页文本：两页，每页 2 行中文 + 1 行中英数字混排。
    pub const SAMPLE_PAGES: [&[&str]; 2] = [
        &[
            "深秋的校园格外安静，银杏叶铺满了整条林荫道。",
            "图书馆的阅览室里，学生们埋头写着各自的报告。",
            "docforge text-layer 2026 page-001 测试行",
        ],
        &[
            "会议室的灯一直亮到深夜，方案改到第七稿才定。",
            "他推开门，把打印好的文件放在桌上就走了。",
            "PDF fast-path probe 12345 abcDEF 第二页-002",
        ],
    ];

    fn font_candidates() -> Vec<std::path::PathBuf> {
        let mut v = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            v.push(std::path::PathBuf::from(home).join(".local/share/fonts/msyh.ttc"));
        }
        v.push("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc".into());
        v.push("/usr/share/fonts/wps-office/HYZYB5.ttf".into());
        // 文泉驿正黑：Debian/Ubuntu 默认 CJK 字体路径（ttc 抽面同 Noto）
        v.push("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc".into());
        v
    }

    /// 生成两页中文文字层样例 PDF。无可用 CJK 字体时返回 None（测试侧 skip）。
    pub fn generate_text_pdf(path: &Path) -> Option<String> {
        let (mut doc, used) = doc_with_cjk_font()?;
        for lines in SAMPLE_PAGES {
            doc.add_page(text_page(lines));
        }
        doc.save(path).ok()?;
        Some(used)
    }

    /// 生成混合样例：页 1 文字层，页 2 纯图像（模拟扫描页）。
    /// 分类器应判出"页 2 需要 OCR"。
    pub fn generate_mixed_pdf(path: &Path) -> Option<String> {
        let (mut doc, used) = doc_with_cjk_font()?;
        doc.add_page(text_page(SAMPLE_PAGES[0]));

        // 合成"扫描页"：白底 + 深灰条带（模拟文字块）JPEG，内存编码
        let mut img = image::RgbImage::new(1200, 1600);
        for (_, _, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([248, 248, 244]);
        }
        for (x0, y0, w, h) in [
            (100, 150, 1000, 40),
            (100, 260, 800, 40),
            (100, 400, 1000, 300),
        ] {
            for y in y0..y0 + h {
                for x in x0..x0 + w {
                    img.put_pixel(x, y, image::Rgb([40, 40, 40]));
                }
            }
        }
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .ok()?;
        let scan = Image::from_jpeg_data(jpeg).ok()?;
        let mut page = Page::a4();
        page.add_image("scan", scan);
        page.draw_image("scan", 40.0, 40.0, 515.0, 762.0).ok()?;
        doc.add_page(page);

        doc.save(path).ok()?;
        Some(used)
    }

    /// 生成页内混合样例（M4）：单页 A4，上半页 12 行文字层（循环复用
    /// SAMPLE_PAGES 两页文本，y 780→615），下半页嵌一个**部分区域**大图
    /// （400×300pt @ (60,40)，与文字层无重叠）。与 generate_mixed_pdf 的
    /// 整页图不同：整页图会被页级分类抓走整页 OCR，本件的关键是图只占
    /// 部分区域——页级分类应判为文字层页，图区交给页内分治。
    /// 位图 800×600px（480K < pdf-inspector template-image 阈值 500K）+
    /// 文字算子 12（≥ 分类器门槛 max(3,10)），双保险确保页不被判扫描页。
    pub fn generate_intra_mixed_pdf(path: &Path) -> Option<String> {
        let (mut doc, used) = doc_with_cjk_font()?;
        let mut page = Page::a4();
        let ctx = page.text();
        let mut lines: Vec<&str> = Vec::new();
        for _ in 0..2 {
            lines.extend_from_slice(SAMPLE_PAGES[0]);
            lines.extend_from_slice(SAMPLE_PAGES[1]);
        }
        for (i, line) in lines.iter().enumerate() {
            let _ = ctx
                .set_font(Font::Custom("CJK".into()), 12.0)
                .at(60.0, 780.0 - 15.0 * i as f64)
                .write(line);
        }
        // 部分区域大图：白底 + 深灰条带（给 OCR 留可识别纹理），内存 JPEG
        let mut img = image::RgbImage::new(800, 600);
        for (_, _, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([248, 248, 244]);
        }
        for (x0, y0, w, h) in [(80, 80, 640, 36), (80, 200, 500, 36), (80, 320, 640, 220)] {
            for y in y0..y0 + h {
                for x in x0..x0 + w {
                    img.put_pixel(x, y, image::Rgb([40, 40, 40]));
                }
            }
        }
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .ok()?;
        let region = Image::from_jpeg_data(jpeg).ok()?;
        page.add_image("region", region);
        page.draw_image("region", 60.0, 40.0, 400.0, 300.0).ok()?;
        doc.add_page(page);

        doc.save(path).ok()?;
        Some(used)
    }

    /// 一页文字：A4，行高 30pt，从 y=780 向下排。
    fn text_page(lines: &[&str]) -> Page {
        let mut page = Page::a4();
        let ctx = page.text();
        for (i, line) in lines.iter().enumerate() {
            let _ = ctx
                .set_font(Font::Custom("CJK".into()), 14.0)
                .at(60.0, 780.0 - 30.0 * i as f64)
                .write(line);
        }
        page
    }

    /// 构建注册了覆盖样例全部字符的 CJK 字体的 Document。
    /// 逐候选尝试：注册 → 查缺字形 → 缺字/失败换下一个。ttc 集合先抽第 0 面。
    fn doc_with_cjk_font() -> Option<(Document, String)> {
        let all_text: String = SAMPLE_PAGES
            .iter()
            .flat_map(|p| p.iter())
            .flat_map(|s| s.chars())
            .collect();
        for cand in font_candidates() {
            if !cand.is_file() {
                continue;
            }
            let Ok(raw) = std::fs::read(&cand) else { continue };
            let data = if is_ttc(&raw) {
                match extract_ttc_face(&raw, 0) {
                    Some(d) => d,
                    None => continue,
                }
            } else {
                raw
            };
            let mut doc = Document::new();
            if doc.add_font_from_bytes("CJK", data).is_err() {
                continue;
            }
            if doc.font_missing_glyphs("CJK", &all_text).is_empty() {
                return Some((doc, cand.to_string_lossy().into_owned()));
            }
        }
        None
    }

    fn is_ttc(data: &[u8]) -> bool {
        data.len() >= 4 && &data[0..4] == b"ttcf"
    }

    /// 从 TrueType Collection 抽出第 `index` 个成员为单体 sfnt。
    ///
    /// ttcf 头 = 'ttcf' + 版本 + 成员数 + 各成员偏移；每个成员本身是完整
    /// sfnt（头 + 表目录 + 表数据）。抽面 = 依偏移读出成员的表目录，重建
    /// 单体文件（表 offset 重写、4 字节对齐；PDF 嵌入不校验内部 checksum，
    /// 置 0 即可——oxidize-pdf 不支持 .ttc，故必须抽面）。
    fn extract_ttc_face(data: &[u8], index: u32) -> Option<Vec<u8>> {
        if data.len() < 12 {
            return None;
        }
        let count = u32::from_be_bytes(data[8..12].try_into().ok()?) as usize;
        let offset_entry = 12 + index as usize * 4;
        if index as usize >= count || offset_entry + 4 > data.len() {
            return None;
        }
        let member_off = u32::from_be_bytes(data[offset_entry..offset_entry + 4].try_into().ok()?)
            as usize;
        if member_off + 12 > data.len() {
            return None;
        }
        let num_tables =
            u16::from_be_bytes(data[member_off + 4..member_off + 6].try_into().ok()?) as usize;
        let dir_len = 12 + num_tables * 16;
        if member_off + dir_len > data.len() {
            return None;
        }
        // 各表 (tag, 数据切片)
        let mut tables: Vec<([u8; 4], &[u8])> = Vec::with_capacity(num_tables);
        for t in 0..num_tables {
            let rec = member_off + 12 + t * 16;
            let tag = [data[rec], data[rec + 1], data[rec + 2], data[rec + 3]];
            let off = u32::from_be_bytes(data[rec + 8..rec + 12].try_into().ok()?) as usize;
            let len = u32::from_be_bytes(data[rec + 12..rec + 16].try_into().ok()?) as usize;
            let end = off.checked_add(len)?;
            if end > data.len() {
                return None;
            }
            tables.push((tag, &data[off..end]));
        }
        // 重建单体 sfnt：sfnt 头原样 + 表目录（offset 重写、checksum 置 0）+ 表数据
        let padded_dir = (dir_len + 3) & !3;
        let total: usize = padded_dir
            + tables
                .iter()
                .map(|(_, d)| (d.len() + 3) & !3)
                .sum::<usize>();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&data[member_off..member_off + 12]);
        let mut cursor = padded_dir;
        for (tag, d) in &tables {
            out.extend_from_slice(tag);
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&(cursor as u32).to_be_bytes());
            out.extend_from_slice(&(d.len() as u32).to_be_bytes());
            cursor += (d.len() + 3) & !3;
        }
        out.resize(padded_dir, 0);
        for (_, d) in &tables {
            out.extend_from_slice(d);
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// 金测用例
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BlockSource;

    /// 样例输出统一放 target/ 下，目录随进程隔离避免并行互踩。
    fn probe_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("pdf-m1-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// pdftoppm 依赖用例的串行闸：PATH 操纵是进程级全局态，且子进程按 PATH
    /// 解析，并行跑会互踩（M4 页内分治用例也渲染，与其共享此闸）。
    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// pdftoppm 是否可用（缺 poppler-utils 的环境可用 ghostscript 顶替，
    /// 见 prepare_renderer；对齐字体缺失的 skip 策略）。须持 PATH_LOCK 后调用。
    fn pdftoppm_available() -> bool {
        Command::new("pdftoppm").arg("-v").output().is_ok()
    }

    /// 无真 pdftoppm 时用 ghostscript 顶替（仅测试）：render_pages 只依赖
    /// `pdftoppm -png -r DPI -f F -l L INPUT PREFIX` 的命令面与 `PREFIX-页码.png`
    /// 产物命名，shim 脚本按同参数面调 gs 产出同名 PNG。有真 pdftoppm 优先
    /// 用真的（返回 None）；两者皆无时返回 Err 由调用方 skip。
    /// 限制：gs `%d` 对输出序编号，多页区间渲染的产物名会偏离页码——本套件
    /// 仅 cancel_midway_stops_remaining_ocr 渲染多页区间，且区间自页 1 起
    ///（-f 1 -l 2，%d 序号恰与页码重合），不受影响；其余用例全部单页渲染
    ///（-f N -l N）。
    fn prepare_renderer() -> std::result::Result<Option<PathBuf>, &'static str> {
        if pdftoppm_available() {
            return Ok(None);
        }
        const GS_SHIM: &str = r#"#!/bin/sh
# docforge 测试 shim：pdftoppm 命令面 → ghostscript 单页渲染
dpi=72; first=1; last=1; rest=""
while [ $# -gt 0 ]; do
  case "$1" in
    -png) ;;
    -r) shift; dpi=$1 ;;
    -f) shift; first=$1 ;;
    -l) shift; last=$1 ;;
    *) rest="$rest $1" ;;
  esac
  shift
done
set -- $rest
gs -q -dNOPAUSE -dBATCH -sDEVICE=png16m -r"$dpi" \
  -dFirstPage="$first" -dLastPage="$last" \
  -sOutputFile="$2-%d.png" "$1" >/dev/null 2>&1
"#;
        if Command::new("gs").arg("--version").output().is_err() {
            return Err("缺 pdftoppm（poppler-utils）且缺 ghostscript，渲染依赖用例不跑");
        }
        let dir = std::env::temp_dir().join(format!("docforge-gs-shim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|_| "gs shim 目录创建失败")?;
        let script = dir.join("pdftoppm");
        std::fs::write(&script, GS_SHIM).map_err(|_| "gs shim 写入失败")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .map_err(|_| "gs shim 加执行位失败")?;
        }
        Ok(Some(dir))
    }

    /// 回归：assemble 曾漏赋 Page.blocks（M4 DjVu 工人发现），Document 层
    /// 必须携带块（emit/翻译单元/RAG 全都消费 blocks）。
    #[test]
    fn assembled_document_has_blocks() {
        let raw = crate::plugin::RawDoc {
            pages: vec![crate::plugin::RawPage {
                name: Some("1".into()),
                items: vec![
                    crate::plugin::RawTextItem {
                        text: "第一行".into(),
                        bbox: Some(crate::model::Rect { x0: 1.0, y0: 1.0, x1: 9.0, y1: 5.0 }),
                        source: crate::model::BlockSource::TextLayer,
                        ..Default::default()
                    },
                ],
                coord_units: Some(crate::model::CoordUnits::Pt),
                ..Default::default()
            }],
            ..Default::default()
        };
        let doc = PdfPlugin.assemble(&raw);
        assert_eq!(doc.pages[0].blocks.len(), 1);
        assert_eq!(doc.pages[0].blocks[0].content, "第一行");
        assert_eq!(doc.pages[0].blocks[0].source, crate::model::BlockSource::TextLayer);
        assert_eq!(doc.pages[0].text, "第一行");
    }

    /// 归一化编辑距离相似度。
    fn similarity(a: &str, b: &str) -> f64 {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let max = a.len().max(b.len());
        if max == 0 {
            return 1.0;
        }
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut cur = vec![0usize; b.len() + 1];
        for i in 1..=a.len() {
            cur[0] = i;
            for j in 1..=b.len() {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        1.0 - (prev[b.len()] as f64) / (max as f64)
    }

    #[test]
    fn text_layer_pdf_routes_all_pages_to_textlayer() {
        let dir = probe_dir("text");
        let pdf = dir.join("sample.pdf");
        let Some(font) = sample::generate_text_pdf(&pdf) else {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        };

        let raw = parse_pdf(&pdf, &PdfOptions::default()).expect("路由失败");
        assert_eq!(raw.pages.len(), 2, "两页全是文字层（字体：{font}）");

        for (i, rp) in raw.pages.iter().enumerate() {
            assert!(!rp.items.is_empty(), "第 {} 页提取为空", i + 1);
            assert_eq!(rp.coord_units, Some(CoordUnits::Pt));
            for exp in sample::SAMPLE_PAGES[i] {
                let best = rp
                    .items
                    .iter()
                    .map(|it| similarity(exp, &it.text))
                    .fold(0.0f64, f64::max);
                assert!(
                    best >= 0.98,
                    "第 {} 页期望行 {exp:?} 最相似度 {best:.3} < 0.98，实际行集：{:?}",
                    i + 1,
                    rp.items.iter().map(|it| &it.text).collect::<Vec<_>>()
                );
            }
            for it in &rp.items {
                assert_eq!(it.source, BlockSource::TextLayer);
                assert!(it.confidence.is_none(), "文字层行不带置信度");
                assert!(it.quad.is_none(), "文字层行不带四点框");
                let Some(b) = it.bbox else { panic!("文字层行应有 bbox") };
                assert!(b.x0 <= b.x1 && b.y0 <= b.y1, "bbox 自洽：{b:?}");
                assert!(b.x0 > 0.0 && b.y1 > 0.0, "PDF 点空间坐标应为正：{b:?}");
            }
        }
        assert!(!raw.needs_ocr, "纯文字层件不应标记 needs_ocr");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mixed_pdf_auto_without_engine_reports_unsupported() {
        let dir = probe_dir("auto-noeng");
        let pdf = dir.join("mixed.pdf");
        if sample::generate_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let err = parse_pdf(&pdf, &PdfOptions::default()).expect_err("无引擎应报错");
        assert_eq!(err.code, ErrorCode::Unsupported, "实际：{err:?}");
        assert!(err.message.contains("需 OCR 引擎"), "实际：{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mixed_pdf_off_mode_skips_scan_pages() {
        let dir = probe_dir("off");
        let pdf = dir.join("mixed.pdf");
        if sample::generate_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let opts = PdfOptions { mode: OcrMode::Off, ..Default::default() };
        let raw = parse_pdf(&pdf, &opts).expect("Off 模式不应失败");
        assert_eq!(raw.pages.len(), 2);
        assert!(!raw.pages[0].items.is_empty(), "页 1 文字层正常提取");
        assert!(raw.pages[1].items.is_empty(), "页 2 扫描页不出块");
        assert_eq!(raw.pages[0].coord_units, Some(CoordUnits::Pt));
        assert!(raw.needs_ocr, "扫描页被跳过应标记 needs_ocr");
        let w = raw.meta.get("warnings").expect("应有告警");
        assert!(w.contains("第 2 页") && w.contains("未 OCR"), "实际告警：{w}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn on_mode_requires_engine_for_all_pages() {
        let dir = probe_dir("on-noeng");
        let pdf = dir.join("sample.pdf");
        if sample::generate_text_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let opts = PdfOptions { mode: OcrMode::On, ..Default::default() };
        let err = parse_pdf(&pdf, &opts).expect_err("On 模式无引擎应报错");
        assert!(err.message.contains("需 OCR 引擎"), "实际：{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_error_is_unsupported() {
        let err = parse_pdf(
            Path::new("/nonexistent/docforge/definitely-missing.pdf"),
            &PdfOptions::default(),
        )
        .expect_err("缺文件应报错");
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert!(err.message.contains("文件不存在"), "实际：{}", err.message);
    }

    /// PATH 操纵使 pdftoppm 不可见：验证友好错误（NotFound 分支）。
    /// 本用例会临时改写进程 PATH——依赖子进程的用例（含 M4 页内分治）经
    /// PATH_LOCK 与其互斥，无并行互踩。
    #[test]
    fn pdftoppm_missing_has_friendly_error() {
        let _guard = PATH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = probe_dir("noppm");
        let pdf = dir.join("mixed.pdf");
        if sample::generate_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        struct NoopEngine;
        impl crate::ocr::OcrEngine for NoopEngine {
            fn name(&self) -> &str {
                "noop"
            }
            fn recognize(
                &self,
                _image: &image::RgbaImage,
            ) -> std::result::Result<Vec<crate::ocr::RecognizedLine>, String> {
                Ok(Vec::new())
            }
        }
        let opts = PdfOptions {
            mode: OcrMode::Auto,
            engine: Some(std::sync::Arc::new(NoopEngine)),
            ..Default::default()
        };
        let old = std::env::var_os("PATH");
        std::env::set_var("PATH", "");
        let result = parse_pdf(&pdf, &opts);
        match old {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        let err = result.expect_err("pdftoppm 不可见应报错");
        assert!(err.message.contains("找不到 pdftoppm"), "实际：{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 页内分治 Mock 引擎：无视裁剪图内容，固定返回 2 行（crop 局部像素
    /// 坐标、左上原点），校验坐标换算与 Ocr 语义透传（形态对齐 ocr/images.rs
    /// 的 MockEngine）。
    struct MockRegionEngine;
    impl crate::ocr::OcrEngine for MockRegionEngine {
        fn name(&self) -> &str {
            "mock-region"
        }
        fn recognize(
            &self,
            _image: &image::RgbaImage,
        ) -> std::result::Result<Vec<crate::ocr::RecognizedLine>, String> {
            Ok(vec![
                crate::ocr::RecognizedLine {
                    text: "图区识别行一".into(),
                    score: 0.9,
                    quad: [[100.0, 120.0], [500.0, 120.0], [500.0, 160.0], [100.0, 160.0]],
                },
                crate::ocr::RecognizedLine {
                    text: "图区识别行二".into(),
                    score: 0.8,
                    quad: [[100.0, 300.0], [420.0, 300.0], [420.0, 340.0], [100.0, 340.0]],
                },
            ])
        }
    }

    /// 页内分治端到端：intra_page=true + Mock 引擎 → 页 1 同时有文字层行与
    /// Ocr 行，coord_units 统一 Pt，OCR 行 bbox 落在图区 pt 范围（±30pt）内。
    /// 需渲染：有真 pdftoppm 用真的，否则 gs shim 顶替（prepare_renderer）。
    #[test]
    fn intra_page_split_ocr_fills_image_region() {
        let _guard = PATH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let shim = match prepare_renderer() {
            Ok(r) => r,
            Err(msg) => {
                eprintln!("skip：{msg}");
                return;
            }
        };
        let dir = probe_dir("intra-ocr");
        let pdf = dir.join("intra.pdf");
        if sample::generate_intra_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let opts = PdfOptions {
            intra_page: true,
            engine: Some(std::sync::Arc::new(MockRegionEngine)),
            ..Default::default()
        };
        // shim 目录前插 PATH（PATH_LOCK 保证与 PATH 操纵用例互斥），跑完还原
        let old = std::env::var_os("PATH");
        if let Some(d) = &shim {
            let new = match old.as_ref() {
                Some(p) => format!("{}:{}", d.display(), p.to_string_lossy()),
                None => d.display().to_string(),
            };
            std::env::set_var("PATH", new);
        }
        let result = parse_pdf(&pdf, &opts);
        match old {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        let raw = result.expect("页内分治解析失败");
        assert_eq!(raw.pages.len(), 1);
        let page = &raw.pages[0];
        assert_eq!(page.coord_units, Some(CoordUnits::Pt), "混合页统一 Pt");
        let text_rows = page
            .items
            .iter()
            .filter(|i| i.source == BlockSource::TextLayer)
            .count();
        assert!(text_rows >= 3, "文字层行数 {text_rows} < 3，分治前置不成立");
        let ocr: Vec<_> = page
            .items
            .iter()
            .filter(|i| i.source == BlockSource::Ocr)
            .collect();
        assert_eq!(
            ocr.len(),
            2,
            "实际行集：{:?}",
            page.items.iter().map(|i| (&i.source, &i.text)).collect::<Vec<_>>()
        );
        for it in &ocr {
            assert!(it.confidence.unwrap_or(0.0) > 0.0, "OCR 行应带正置信度");
            assert!(it.quad.is_none(), "混合页 OCR 行不出 quad（裁剪局部系，见 crop_and_ocr_region）");
            let b = it.bbox.expect("OCR 行应有 bbox");
            // 图区 pt：x 40–460、y 40–340，±30pt 容差粗断言
            assert!(b.x0 > 10.0 && b.x1 < 490.0, "OCR 行 x 越出图区±30pt：{b:?}");
            assert!(b.y0 > 10.0 && b.y1 < 370.0, "OCR 行 y 越出图区±30pt：{b:?}");
        }
        // 阅读序合并：整页 y1 非升序（文字层在上、两行 OCR 在图区）
        let bboxes: Vec<_> = page.items.iter().filter_map(|i| i.bbox).collect();
        for w in bboxes.windows(2) {
            assert!(w[0].y1 >= w[1].y1, "阅读序破坏：{bboxes:?}");
        }
        // 混合分治页顺手填页面 pt 尺寸（A4 ≈ 595×842，渲染反推）
        let (w, h) = (page.width.unwrap_or(0.0), page.height.unwrap_or(0.0));
        assert!(w > 560.0 && w < 630.0 && h > 810.0 && h < 870.0, "页面尺寸异常：{w}×{h}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// intra_page 缺省（false）与现状等价：同一件纯直提，无 Ocr 行。
    #[test]
    fn intra_page_off_by_default_equivalent() {
        let dir = probe_dir("intra-off");
        let pdf = dir.join("intra.pdf");
        if sample::generate_intra_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let raw = parse_pdf(&pdf, &PdfOptions::default()).expect("默认解析失败");
        assert_eq!(raw.pages.len(), 1);
        assert!(!raw.pages[0].items.is_empty(), "文字层直提不应为空");
        assert!(
            raw.pages[0].items.iter().all(|i| i.source == BlockSource::TextLayer),
            "默认不开 intra_page 不应出现 Ocr 行"
        );
        assert_eq!(raw.pages[0].coord_units, Some(CoordUnits::Pt));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// intra_page 开 + 无引擎：判定为真也不失败，纯直提 + 一条"未 OCR"告警。
    #[test]
    fn intra_page_no_engine_warns_not_fails() {
        let dir = probe_dir("intra-noeng");
        let pdf = dir.join("intra.pdf");
        if sample::generate_intra_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let opts = PdfOptions { intra_page: true, ..Default::default() };
        let raw = parse_pdf(&pdf, &opts).expect("无引擎不应失败");
        assert!(!raw.pages[0].items.is_empty(), "纯直提不应为空");
        assert!(
            raw.pages[0].items.iter().all(|i| i.source == BlockSource::TextLayer),
            "无引擎不应出现 Ocr 行"
        );
        let w = raw.meta.get("warnings").expect("应有告警");
        assert!(w.contains("未 OCR") && w.contains("无引擎"), "实际告警：{w}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 页级进度回调：2 页件按页序逐页回调 (1,2)(2,2)（文字层件无渲染依赖，
    /// 回调点 = 各页组装完成点，计数单调）。
    #[test]
    fn progress_callback_counts_all_pages() {
        let dir = probe_dir("progress");
        let pdf = dir.join("sample.pdf");
        if sample::generate_text_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let calls: std::sync::Arc<std::sync::Mutex<Vec<(u32, u32)>>> = std::sync::Arc::default();
        let cb_calls = calls.clone();
        let opts = PdfOptions {
            on_progress: Some(std::sync::Arc::new(move |done, total| {
                cb_calls.lock().unwrap().push((done, total));
                true
            })),
            ..Default::default()
        };
        let raw = parse_pdf(&pdf, &opts).expect("解析失败");
        assert_eq!(raw.pages.len(), 2);
        assert_eq!(*calls.lock().unwrap(), [(1, 2), (2, 2)], "回调序 = 页序");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 进度回调取消：页 1 完成后返回 false → Ok 返回（不是 Err）、页 1 保留
    /// 块、页 2 出空占位（页号对齐）、warnings 含中止标记。
    #[test]
    fn progress_cancel_keeps_done_pages() {
        let dir = probe_dir("cancel");
        let pdf = dir.join("sample.pdf");
        if sample::generate_text_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let opts = PdfOptions {
            on_progress: Some(std::sync::Arc::new(|done, _total| done < 1)),
            ..Default::default()
        };
        let raw = parse_pdf(&pdf, &opts).expect("中止是 Ok 语义，不应报错");
        assert_eq!(raw.pages.len(), 2, "空占位保持页号对齐");
        assert!(!raw.pages[0].items.is_empty(), "已完成的页 1 保留块");
        assert!(raw.pages[1].items.is_empty(), "页 2 未处理，出空占位");
        assert_eq!(raw.pages[1].name.as_deref(), Some("2"));
        let w = raw.meta.get("warnings").expect("应有中止告警");
        assert!(w.contains("用户中止于第 1 页"), "实际告警：{w}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 计数 Mock 引擎：AtomicUsize 记录 recognize 调用次数，断言取消后
    /// "算力截停"（不止产出截停）。固定返回一行，使 OCR 产出可断言。
    struct CountingEngine {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingEngine {
        fn new() -> Self {
            Self { calls: std::sync::atomic::AtomicUsize::new(0) }
        }
        fn count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl crate::ocr::OcrEngine for CountingEngine {
        fn name(&self) -> &str {
            "counting"
        }
        fn recognize(
            &self,
            _image: &image::RgbaImage,
        ) -> std::result::Result<Vec<crate::ocr::RecognizedLine>, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![crate::ocr::RecognizedLine {
                text: "计数引擎识别行".into(),
                score: 0.9,
                quad: [[0.0, 0.0], [100.0, 0.0], [100.0, 20.0], [0.0, 20.0]],
            }])
        }
    }

    /// 取消先于扫描页（懒式管线核心断言）：混合件（页 1 文字层 + 页 2 扫描）
    /// 页 1 回调即 false → Ok 返回、页 2 空占位，且 Mock 引擎 recognize 调用
    /// 次数 == 0——取消截停产出，也截停算力（页 2 的区间渲染与 OCR 整段
    /// 不发生）。页 1 为文字层页，全程无渲染依赖。
    #[test]
    fn cancel_before_scan_pages_skips_ocr_entirely() {
        let dir = probe_dir("cancel-noocr");
        let pdf = dir.join("mixed.pdf");
        if sample::generate_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let engine = std::sync::Arc::new(CountingEngine::new());
        let opts = PdfOptions {
            mode: OcrMode::Auto,
            engine: Some(engine.clone()),
            on_progress: Some(std::sync::Arc::new(|_done, _total| false)),
            ..Default::default()
        };
        let raw = parse_pdf(&pdf, &opts).expect("取消是 Ok 语义，不应报错");
        assert_eq!(engine.count(), 0, "核心断言：取消后扫描页完全没进 OCR 引擎");
        assert_eq!(raw.pages.len(), 2, "空占位保持页号对齐");
        assert!(!raw.pages[0].items.is_empty(), "已产出的页 1 文字层保留");
        assert!(raw.pages[1].items.is_empty(), "页 2 未处理，出空占位");
        assert_eq!(raw.pages[1].name.as_deref(), Some("2"));
        let w = raw.meta.get("warnings").expect("应有中止告警");
        assert!(w.contains("用户中止于第 1 页"), "实际告警：{w}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 取消截停区间内余页 OCR：两扫描页件（On 模式全页 raster，区间 [1,2]
    /// 渲染一次完成）页 1 OCR + 回调 false → 页 2 不再进引擎（recognize 恰好
    /// 1 次），页 2 空占位。渲染依赖同 intra 用例（真 pdftoppm 或 gs shim
    /// 顶替；区间自页 1 起，shim 的 %d 序号与页码重合）。
    #[test]
    fn cancel_midway_stops_remaining_ocr() {
        let _guard = PATH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let shim = match prepare_renderer() {
            Ok(r) => r,
            Err(msg) => {
                eprintln!("skip：{msg}");
                return;
            }
        };
        let dir = probe_dir("cancel-mid");
        let pdf = dir.join("sample.pdf");
        if sample::generate_text_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let engine = std::sync::Arc::new(CountingEngine::new());
        let opts = PdfOptions {
            mode: OcrMode::On,
            engine: Some(engine.clone()),
            on_progress: Some(std::sync::Arc::new(|done, _total| done < 1)),
            ..Default::default()
        };
        // shim 目录前插 PATH（PATH_LOCK 保证与 PATH 操纵用例互斥），跑完还原
        let old = std::env::var_os("PATH");
        if let Some(d) = &shim {
            let new = match old.as_ref() {
                Some(p) => format!("{}:{}", d.display(), p.to_string_lossy()),
                None => d.display().to_string(),
            };
            std::env::set_var("PATH", new);
        }
        let result = parse_pdf(&pdf, &opts);
        match old {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        let raw = result.expect("取消是 Ok 语义，不应报错");
        assert_eq!(engine.count(), 1, "区间内逐页截停：页 2 不再进 OCR 引擎");
        assert_eq!(raw.pages.len(), 2);
        assert!(!raw.pages[0].items.is_empty(), "页 1 OCR 已产出保留");
        assert_eq!(
            raw.pages[0].coord_units,
            Some(CoordUnits::Px { dpi: DEFAULT_DPI as f32 }),
            "On 模式页 1 走渲染+OCR 坐标系"
        );
        assert!(raw.pages[1].items.is_empty(), "页 2 未 OCR，空占位");
        assert_eq!(raw.pages[1].name.as_deref(), Some("2"));
        let w = raw.meta.get("warnings").expect("应有中止告警");
        assert!(w.contains("用户中止于第 1 页"), "实际告警：{w}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// keep_raster_pages：扫描页渲染图以 PNG 进 Page.images（assemble 透传），
    /// 与 OCR 消费同一渲染图；默认关闭时 images 为空。渲染依赖同 intra 用例
    ///（真 pdftoppm 或 gs shim 顶替）；顺带断言扫描页 OCR 行 quad 直通
    ///（RawTextItem → assemble → Block.quad 全程保留）。
    #[test]
    fn keep_raster_pages_attaches_png() {
        let _guard = PATH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let shim = match prepare_renderer() {
            Ok(r) => r,
            Err(msg) => {
                eprintln!("skip：{msg}");
                return;
            }
        };
        let dir = probe_dir("keep-raster");
        let pdf = dir.join("mixed.pdf");
        if sample::generate_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let mk_opts = |keep: bool| PdfOptions {
            engine: Some(std::sync::Arc::new(MockRegionEngine)),
            keep_raster_pages: keep,
            ..Default::default()
        };
        // shim 目录前插 PATH（PATH_LOCK 保证与 PATH 操纵用例互斥），两轮解析
        //（开/关）跑完统一还原
        let old = std::env::var_os("PATH");
        if let Some(d) = &shim {
            let new = match old.as_ref() {
                Some(p) => format!("{}:{}", d.display(), p.to_string_lossy()),
                None => d.display().to_string(),
            };
            std::env::set_var("PATH", new);
        }
        let on = parse_pdf(&pdf, &mk_opts(true));
        let off = parse_pdf(&pdf, &mk_opts(false));
        match old {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        let on = on.expect("开启解析失败");
        let off = off.expect("关闭解析失败");

        // RawPage 层：渲染页图 ext="png"、px 记录渲染尺寸；OCR 行 quad 直通
        let raw_page = &on.pages[1];
        assert_eq!(raw_page.images.len(), 1, "扫描页恰一张渲染页图");
        let raw_im = &raw_page.images[0];
        assert_eq!(raw_im.ext, "png");
        let (pw, ph) = raw_im.px.expect("渲染页图应带像素尺寸");
        assert!(pw > 0 && ph > 0);
        assert!(
            raw_page.items.iter().all(|i| i.quad.is_some()),
            "扫描页 OCR 行四点框直通"
        );
        assert!(on.pages[0].images.is_empty(), "文字层页不产渲染页图");

        // IR 层：assemble 透传 → Page.images[0] 为 PNG（magic 头）
        let doc = PdfPlugin.assemble(&on);
        assert!(doc.pages[0].images.is_empty());
        assert_eq!(doc.pages[1].images.len(), 1);
        let im = &doc.pages[1].images[0];
        assert!(!im.data.is_empty(), "渲染页字节非空");
        assert!(
            im.data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            "应为 PNG 头"
        );
        assert_eq!(im.bbox, None, "整页渲染图无定位框");
        // OCR 行 quad 经 assemble 透传至 Block.quad
        let ocr_block = doc.pages[1]
            .blocks
            .iter()
            .find(|b| b.source == BlockSource::Ocr)
            .expect("应有 Ocr 块");
        assert_eq!(
            ocr_block.quad,
            Some([[100.0, 120.0], [500.0, 120.0], [500.0, 160.0], [100.0, 160.0]]),
            "MockRegionEngine 首行四点框原样保留"
        );

        // 关：默认不保留渲染页图
        let doc_off = PdfPlugin.assemble(&off);
        assert!(doc_off.pages[1].images.is_empty(), "默认不保留渲染页图");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// needs_intra_page_split 判定单测：大图+少文字→false（防越权）；
    /// 大图+文字够+无覆盖→true；小图/扁条→false；文字覆盖图 30%+→false。
    fn img_item(x: f32, y: f32, w: f32, h: f32) -> pdf_inspector::TextItem {
        pdf_inspector::TextItem {
            text: "[Image: t]".into(),
            x,
            y,
            width: w,
            height: h,
            font: String::new(),
            font_tag: String::new(),
            legacy_symbol_rewrite: false,
            font_size: 0.0,
            page: 1,
            is_bold: false,
            is_italic: false,
            font_weight: None,
            bold_source: None,
            fixed_pitch: None,
            fill_color: None,
            stroke_color: None,
            render_mode: None,
            is_underline: false,
            is_strikeout: false,
            rotation: 0.0,
            advance_known: true,
            item_type: ItemType::Image,
            mcid: None,
            baseline_shift: 0.0,
        }
    }

    fn text_line(x0: f32, y0: f32, x1: f32, y1: f32) -> RawTextItem {
        RawTextItem {
            text: "行".into(),
            bbox: Some(Rect { x0, y0, x1, y1 }),
            source: BlockSource::TextLayer,
            ..Default::default()
        }
    }

    #[test]
    fn needs_intra_page_split_unit() {
        // 大图：400×300 = 120_000 pt² ≥ 96_000，高 300 ≥ 60
        let big = img_item(60.0, 40.0, 400.0, 300.0);
        // 文字够：3 行，全在图外（上半页）
        let lines = [
            text_line(0.0, 600.0, 500.0, 614.0),
            text_line(0.0, 620.0, 500.0, 634.0),
            text_line(0.0, 640.0, 500.0, 654.0),
        ];
        // 大图 + 少文字 → false（文字太少的页归页级路由管，防越权）
        assert!(!needs_intra_page_split(&lines[..2], std::slice::from_ref(&big)));
        // 大图 + 文字够 + 无覆盖 → true
        assert!(needs_intra_page_split(&lines, std::slice::from_ref(&big)));
        // 小图（面积不足）→ false
        let small = img_item(60.0, 40.0, 100.0, 40.0);
        assert!(!needs_intra_page_split(&lines, std::slice::from_ref(&small)));
        // 扁宽装饰条：面积够（100_000 pt²）但高 < 60 → false
        let strip = img_item(0.0, 40.0, 2000.0, 50.0);
        assert!(!needs_intra_page_split(&lines, &[strip]));
        // 文字行覆盖图 41%（>30%）→ false（文字层已覆盖，防双份）
        // 覆盖行 (70,50)-(450,180)：重叠 380×130 = 49_400 / 120_000 ≈ 41%
        let cover = [text_line(70.0, 50.0, 450.0, 180.0), lines[1].clone(), lines[2].clone()];
        assert!(!needs_intra_page_split(&cover, &[big]));
    }

    /// 裁剪探针引擎：把拿到的裁剪图尺寸作为"识别文本"返回，并把行画在
    /// 裁剪图 (0,0)-(10,10)——借以精确校验 pt→px 裁剪矩形与 px→pt 翻转换算。
    struct CropProbeEngine;
    impl crate::ocr::OcrEngine for CropProbeEngine {
        fn name(&self) -> &str {
            "crop-probe"
        }
        fn recognize(
            &self,
            image: &image::RgbaImage,
        ) -> std::result::Result<Vec<crate::ocr::RecognizedLine>, String> {
            Ok(vec![crate::ocr::RecognizedLine {
                text: format!("{}x{}", image.width(), image.height()),
                score: 0.9,
                quad: [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
            }])
        }
    }

    /// crop_and_ocr_region 坐标换算单测（无渲染依赖）：A4@200dpi 整页图
    /// 1654×2339，图区 pt (60,40)-(460,340) → 裁剪 px x 166→1278、y 1394→2228
    /// （1112×834）；探针行（裁剪图左上 10×10）→ 整页 pt x ≈59.76–63.36、
    /// y ≈336.6–340.2（上缘贴图区上缘 340，y 已翻转）。
    #[test]
    fn crop_and_ocr_region_maps_coords() {
        let page = image::RgbaImage::new(1654, 2339);
        let h_pt = 2339.0 * 72.0 / 200.0; // 842.04，与渲染反推一致
        let rect = Rect { x0: 60.0, y0: 40.0, x1: 460.0, y1: 340.0 };
        let items = crop_and_ocr_region(&page, rect, 200, h_pt, &CropProbeEngine).unwrap();
        assert_eq!(items.len(), 1);
        // 裁剪矩形推导：x 166..1278、y 1394..2228
        assert_eq!(items[0].text, "1112x834", "裁剪 px 尺寸不符");
        // 翻转换算：探针行贴裁剪图左上 → 整页 pt 贴图区左上（60,340）
        let b = items[0].bbox.expect("OCR 行应有 bbox");
        assert!((b.x0 - 59.76).abs() < 1.5, "x0 实际 {}", b.x0);
        assert!((b.x1 - 63.36).abs() < 1.5, "x1 实际 {}", b.x1);
        assert!((b.y0 - 336.6).abs() < 1.5, "y0 实际 {}", b.y0);
        assert!((b.y1 - 340.2).abs() < 1.5, "y1 实际 {}（应≈图区上缘 340）", b.y1);
        assert!(b.y0 < b.y1, "翻转后 y 语义应保持 y0=下缘 < y1=上缘");
    }

    /// 阅读序合并单测：y1 降序（页面上方在前），同 y 按 x0 升序。
    #[test]
    fn merge_by_reading_order_sorts_y_desc_x_asc() {
        let top = text_line(100.0, 700.0, 200.0, 714.0);
        let low_left = text_line(10.0, 300.0, 90.0, 314.0);
        let low_right = text_line(300.0, 300.0, 380.0, 314.0);
        let merged =
            merge_by_reading_order(vec![top.clone()], vec![low_right.clone(), low_left.clone()]);
        assert_eq!(merged[0].bbox, top.bbox, "y1 大的行在前");
        assert_eq!(merged[1].bbox, low_left.bbox, "同 y 按 x0 升序");
        assert_eq!(merged[2].bbox, low_right.bbox);
    }

    /// 混合件 Auto + oar 引擎端到端：`cargo test --features ocr-oar -- --ignored`。
    /// 需要本机 pdftoppm 与 ~/.oar 模型缓存（首次运行自动下载）。
    #[test]
    #[cfg(feature = "ocr-oar")]
    #[ignore = "需要 oar-ocr 模型与系统 pdftoppm"]
    fn mixed_pdf_auto_ocr_end_to_end() {
        let dir = probe_dir("ocr-e2e");
        let pdf = dir.join("mixed.pdf");
        if sample::generate_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let opts = PdfOptions {
            engine: Some(crate::ocr::oar::OarOcrEngine::shared()),
            ..Default::default()
        };
        let raw = parse_pdf(&pdf, &opts).expect("端到端失败");
        assert_eq!(raw.pages.len(), 2);
        // 页 1：文字层
        assert_eq!(raw.pages[0].coord_units, Some(CoordUnits::Pt));
        assert!(raw.pages[0].items.iter().all(|i| i.source == BlockSource::TextLayer));
        // 页 2：OCR（条带是纯色块，识别行可能为空——至少坐标/来源语义正确）
        assert_eq!(
            raw.pages[1].coord_units,
            Some(CoordUnits::Px { dpi: DEFAULT_DPI as f32 })
        );
        for it in &raw.pages[1].items {
            assert_eq!(it.source, BlockSource::Ocr);
            assert!(it.confidence.unwrap_or(0.0) > 0.0, "OCR 行应带正置信度");
        }
        println!("页 2 OCR 行数：{}", raw.pages[1].items.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 等价性金测（核心）：混合件（页 1 文字层 + 页 2 扫描页，Mock 引擎）
    /// 分别走批式 parse_pdf→assemble→finalize 与深度流式 parse_streaming
    ///（收集），断言逐页相等（PartialEq 全字段）与 meta 统计一致——
    /// produce_page 抽取 + 流式入口是行为等价重构。
    #[test]
    fn pdf_streaming_equals_batch() {
        let _guard = PATH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let shim = match prepare_renderer() {
            Ok(r) => r,
            Err(msg) => {
                eprintln!("skip：{msg}");
                return;
            }
        };
        let dir = probe_dir("stream-eq");
        let pdf = dir.join("mixed.pdf");
        if sample::generate_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let opts = PdfOptions {
            engine: Some(std::sync::Arc::new(MockRegionEngine)),
            ..Default::default()
        };
        // shim 目录前插 PATH（PATH_LOCK 保证与 PATH 操纵用例互斥），两轮解析
        //（批式/流式）跑完统一还原
        let old = std::env::var_os("PATH");
        if let Some(d) = &shim {
            let new = match old.as_ref() {
                Some(p) => format!("{}:{}", d.display(), p.to_string_lossy()),
                None => d.display().to_string(),
            };
            std::env::set_var("PATH", new);
        }
        let batch = parse_pdf(&pdf, &opts);
        let mut collected: Vec<Page> = Vec::new();
        let stream = parse_streaming(&pdf, &opts, &mut |page: Page| {
            collected.push(page);
            true
        });
        match old {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        // engine 组装段会回填 file_type（流式入口自述 "pdf"，同一语义位），
        // 批式基线补齐同值再比
        let mut batch_doc = PdfPlugin.assemble(&batch.expect("批式解析失败")).finalize();
        batch_doc.meta.file_type = "pdf".to_string();
        let stream_meta = stream.expect("流式解析失败");

        // 逐页相等（Debug/PartialEq 全字段：blocks/text/images/coord_units/name）
        assert_eq!(
            collected.len(),
            batch_doc.pages.len(),
            "投递页数 = 批式页数"
        );
        for (i, (sp, bp)) in collected.iter().zip(batch_doc.pages.iter()).enumerate() {
            assert_eq!(sp, bp, "第 {} 页流式与批式不相等", i + 1);
        }
        // meta 统计一致（file_type/page_count/text_length/image_count/extra）
        assert_eq!(stream_meta, batch_doc.meta, "meta 统计一致");
        // 抽查路由语义（等价性不掩盖页级路由正确性）：页 1 文字层 Pt、
        // 页 2 扫描页 OCR Px 且有 Mock 识别块
        assert_eq!(collected[0].coord_units, Some(CoordUnits::Pt));
        assert_eq!(
            collected[1].coord_units,
            Some(CoordUnits::Px { dpi: DEFAULT_DPI as f32 })
        );
        assert!(
            collected[1].blocks.iter().all(|b| b.source == BlockSource::Ocr),
            "页 2 全部为 OCR 块"
        );
        assert!(!collected[1].blocks.is_empty(), "Mock 引擎扫描页应有识别块");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 深度流式的单页内存语义（不真测内存）：keep_raster_pages 流式收页，
    /// 扫描页 data 完整（PNG 头 + 与批式逐字节一致——同一渲染图编码）且
    /// meta.image_count 正确（= 已投递页图数）。
    #[test]
    fn pdf_streaming_keeps_single_page_memory_semantics() {
        let _guard = PATH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let shim = match prepare_renderer() {
            Ok(r) => r,
            Err(msg) => {
                eprintln!("skip：{msg}");
                return;
            }
        };
        let dir = probe_dir("stream-raster");
        let pdf = dir.join("mixed.pdf");
        if sample::generate_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let opts = PdfOptions {
            engine: Some(std::sync::Arc::new(MockRegionEngine)),
            keep_raster_pages: true,
            ..Default::default()
        };
        let old = std::env::var_os("PATH");
        if let Some(d) = &shim {
            let new = match old.as_ref() {
                Some(p) => format!("{}:{}", d.display(), p.to_string_lossy()),
                None => d.display().to_string(),
            };
            std::env::set_var("PATH", new);
        }
        let batch = parse_pdf(&pdf, &opts);
        let mut collected: Vec<Page> = Vec::new();
        let stream = parse_streaming(&pdf, &opts, &mut |page: Page| {
            collected.push(page);
            true
        });
        match old {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        // engine 组装段会回填 file_type（流式入口自述 "pdf"），基线补齐同值
        let mut batch_doc = PdfPlugin.assemble(&batch.expect("批式解析失败")).finalize();
        batch_doc.meta.file_type = "pdf".to_string();
        let meta = stream.expect("流式解析失败");

        assert_eq!(collected.len(), 2);
        assert!(collected[0].images.is_empty(), "文字层页不产渲染页图");
        assert_eq!(collected[1].images.len(), 1, "扫描页恰一张渲染页图");
        let data = &collected[1].images[0].data;
        assert!(!data.is_empty(), "渲染页字节非空");
        assert!(
            data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            "应为 PNG 头"
        );
        assert_eq!(
            data,
            &batch_doc.pages[1].images[0].data,
            "流式与批式的渲染页字节一致（同一渲染图 PNG 编码，data 完整）"
        );
        assert_eq!(meta.image_count, 1, "image_count = 已投递页图数");
        assert_eq!(meta, batch_doc.meta, "keep_raster 件 meta 仍逐项一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 深度流式 sink 取消：页 1（文字层）投递后 false → Ok 返回（不报错）、
    /// page_count == 1（被拒页计为已投递）、页 2 扫描页整段不产（Mock 引擎
    /// recognize == 0——sink 取消同样截停算力：区间渲染与 OCR 不发生）、
    /// warnings 记"用户中止于第 1 页"。无渲染依赖（页 2 不渲染）。
    #[test]
    fn pdf_streaming_sink_cancel_stops_compute() {
        let dir = probe_dir("stream-cancel");
        let pdf = dir.join("mixed.pdf");
        if sample::generate_mixed_pdf(&pdf).is_none() {
            eprintln!("skip：无覆盖样例字符的 CJK 系统字体");
            return;
        }
        let engine = std::sync::Arc::new(CountingEngine::new());
        let opts = PdfOptions {
            mode: OcrMode::Auto,
            engine: Some(engine.clone()),
            ..Default::default()
        };
        let mut delivered = 0usize;
        let meta = parse_streaming(&pdf, &opts, &mut |_page: Page| {
            delivered += 1;
            false
        })
        .expect("取消是 Ok 语义，不应报错");
        assert_eq!(delivered, 1, "sink false 即停，余页不投");
        assert_eq!(meta.page_count, 1);
        assert_eq!(engine.count(), 0, "核心断言：sink 取消截停扫描页渲染与 OCR");
        let w = meta.extra.get("warnings").expect("应有中止告警");
        assert!(w.contains("用户中止于第 1 页"), "实际告警：{w}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
