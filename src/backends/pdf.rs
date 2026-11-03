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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use pdf_inspector::extractor::{group_into_lines_preserving_all_text, ItemType};
use pdf_inspector::{DetectionConfig, ScanStrategy};

use crate::error::{DocError, ErrorCode, Result};
use crate::model::{blocks_to_text, Block, BlockKind, BlockSource, CoordUnits, Document, Page,
    Rect};
use crate::ocr::OcrEngine;
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawPage, RawTextItem};
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
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self { mode: OcrMode::Auto, dpi: DEFAULT_DPI, engine: None, intra_page: false }
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

    fn parse(&self, path: &str) -> Result<RawDoc> {
        parse_pdf(Path::new(path), &self.opts)
    }
}

/// 页级路由主流程。
fn parse_pdf(path: &Path, opts: &PdfOptions) -> Result<RawDoc> {
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

    // 文字层页：分类后按需提取（Off 模式 raster 集为空 → 全部页走文字层）
    let text_pages: HashSet<u32> = (1..=page_count).filter(|p| !raster_pages.contains(p)).collect();
    let items_by_page = if text_pages.is_empty() {
        Vec::new()
    } else {
        pdf_inspector::extract_text_with_positions_pages(path, Some(&text_pages))
            .map_err(|e| DocError {
                code: ErrorCode::Corrupt,
                message: format!("文字层提取失败（{}）：{e}", path.display()),
                attempts: vec![],
            })?
    };
    let mut lines_by_page: Vec<Vec<RawTextItem>> = vec![Vec::new(); page_count as usize];
    for line in group_into_lines_preserving_all_text(items_by_page) {
        if let Some(item) = text_line_to_item(&line) {
            lines_by_page[line.page as usize - 1].push(item);
        }
    }

    // 扫描页：连续区间合并渲染（一次 pdftoppm -f/-l），逐页识别
    let mut raster_lines: Vec<Vec<RawTextItem>> = vec![Vec::new(); page_count as usize];
    if !raster_pages.is_empty() {
        let engine = opts.engine.as_ref().ok_or_else(|| DocError {
            code: ErrorCode::Unsupported,
            message: format!(
                "第 {} 页需 OCR 引擎：开启 ocr-oar feature 或在 PdfOptions.engine 注入 OcrEngine",
                raster_pages.iter().min().unwrap()
            ),
            attempts: vec![],
        })?;
        let mut p = 1u32;
        while p <= page_count {
            if !raster_pages.contains(&p) {
                p += 1;
                continue;
            }
            let first = p;
            while p <= page_count && raster_pages.contains(&p) {
                p += 1;
            }
            let last = p - 1;
            let images = render_pages(path, first, last, opts.dpi)?;
            for (i, img) in images.into_iter().enumerate() {
                let page_no = first + i as u32;
                let lines = engine
                    .recognize(&img)
                    .map_err(|e| DocError { code: ErrorCode::Unknown, message: e, attempts: vec![] })?;
                raster_lines[page_no as usize - 1] =
                    lines.into_iter().map(|l| {
                        let bbox = l.axis_rect();
                        RawTextItem {
                            text: l.text,
                            bbox: Some(bbox),
                            source: BlockSource::Ocr,
                            confidence: Some(l.score),
                            ..Default::default()
                        }
                    }).collect();
            }
        }
    }

    // Off 模式：探测出的扫描页不渲染不报错，记告警（页不出块）
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

    // 组装：页序 = 文档序；每页取对应来源的 items
    let mut pages = Vec::with_capacity(page_count as usize);
    for (idx, p) in (1..=page_count).enumerate() {
        let is_raster = raster_pages.contains(&p);
        let items = if is_raster {
            std::mem::take(&mut raster_lines[idx])
        } else {
            std::mem::take(&mut lines_by_page[idx])
        };
        pages.push(RawPage {
            name: Some(p.to_string()),
            items,
            coord_units: Some(if is_raster {
                CoordUnits::Px { dpi: opts.dpi as f32 }
            } else {
                CoordUnits::Pt
            }),
            ..Default::default()
        });
    }

    let needs_ocr = !raster_pages.is_empty() || (opts.mode == OcrMode::Off && !scan_detected.is_empty());
    let mut meta = std::collections::HashMap::new();
    if let Some(w) = warnings.pop() {
        meta.insert("warnings".to_string(), w);
    }
    Ok(RawDoc { pages, meta, needs_ocr })
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
                        confidence: i.confidence,
                        source: i.source,
                        ..Default::default()
                    })
                    .collect();
                let text = blocks_to_text(&blocks, "\n");
                Page {
                    name: rp.name.clone(),
                    blocks,
                    text,
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
    /// 本用例会临时改写进程 PATH——依赖子进程的用例仅此与 #[ignore] 的
    /// OCR 端到端（默认不跑），无并行互踩。
    #[test]
    fn pdftoppm_missing_has_friendly_error() {
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
}
