// DjVu 后端：页级分治（文字层直提 / 无字页渲染+OCR）+ 引擎注入（feature = "djvu"）。
//
// 页级分治**参考 document-parser（B）formats/djvu/loader.rs 的模式重新实现**
//（docparse-plan §2.4-4 / §7：文字页 `page.text()` 直提、无字页渲染；B 的
// loader.rs L167-206 对无字页只输出页面 JPEG 资源，docforge 的差异化位是
// 无字页渲染后接 `OcrEngine` 识别产 Ocr 文本块——与 pdf 后端的扫描页路径
// 同构，OCR 与翻译支撑才是一等能力）。
//
// # 选型结论（M4 实测，判定依据详见任务报告）
//
// 采用 **djvu-rs 0.44**（matyushkin，MIT，活跃），不用 `djvu 0.1.0`
//（bugzmanov，**GPL-3.0**，2026-03 后未更）：
// 1. 文字层结构质量：djvu-rs 的 `TextZone` 自带 `text` 与 `rect`，且解析时
//    已把 DjVu 原生左下原点坐标重映射为**左上原点**（vendor/djvu-rs-0.44.0/
//    src/text.rs L21-23、L462-469），层级 Page/Column/Region/Para/Line/Word/
//    Character 完整、带深度与分配防护（#589/#524）；`djvu 0.1.0` 需要消费方
//    自己用 text_start/text_len 切片并手工翻 y 轴（其 document.rs L56-70）。
// 2. image 0.25 兼容：djvu-rs 渲染输出 `Pixmap{width,height,data: RGBA 行主序}`
//    与 docforge 的 image 0.25 `RgbaImage::from_raw` 零拷贝对接（另有可选
//    `image` feature 提供 ImageDecoder 集成，本后端不需要）。
// 3. 许可：docforge 是 MIT OR Apache-2.0 且计划发布 crates.io（§11/§12），
//    GPL-3.0 的 `djvu 0.1.0` 会污染 djvu feature 的下游合规，一票否决。
//
// # 坐标系契约（风格对齐 pdf.rs 模块头）
//
// DjVu TXTz/TXTa 原生坐标 = **左下原点、y 向上、单位页原生像素**（INFO
// chunk 的 width/height/dpi 语义）；djvu-rs 解析时已按页高统一重映射为
// **左上原点、y 向下**（text.rs L462-469），本后端直接消费，不再翻转：
// - 文字层页 `Page.coord_units = Some(CoordUnits::Px { dpi: 页 dpi })`：
//   行 bbox 直接取 Line zone 的 rect（左上原点像素）；INFO dpi=0（病态值，
//   编码器会钳到 ≥1，但防手工构造件）标注为 `Logical`；
// - OCR 页 `Some(CoordUnits::Px { dpi: 渲染 dpi })`：渲染恒取原生分辨率
//   （scale=1），故渲染 dpi = 页 dpi；quad→bbox 契约与 pdf 后端 OCR 页一致
//   （左上原点像素）。
// 注意：zone rect 在页**原生（未旋转）**像素空间；`Page.rotation` 已透传
// INFO 旋转角，旋转页的文字层 bbox 与渲染 pixmap（显示方向）存在轴交换，
// 消费者按 rotation 换算（旋转 DjVu 罕见，不预先展开）。
//
// rumi 集成增强：无字页 OCR 行的 `RecognizedLine.quad` 直通 RawTextItem/
// Block（旋转保真，与 pdf 后端扫描页路径一致）；文字层路径恒 None。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use djvu_rs::{Document as DjvuDocument, Rotation, TextLayer, TextZone, TextZoneKind};

use crate::error::{DocError, ErrorCode, Result};
use crate::model::{
    blocks_to_text, Block, BlockKind, BlockSource, CoordUnits, Document, Page, Rect,
};
use crate::ocr::OcrEngine;
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawPage, RawTextItem};
use crate::registry::Registry;

/// DjVu 后端选项（页级分治无需三模式：有无文字层由件本身决定）。
#[derive(Clone, Default)]
pub struct DjVuOptions {
    /// OCR 引擎：无文字层页（含文字层为空白的退化页）渲染后识别，
    /// 产 Ocr 块（带 confidence）；`None` 时无字页产空页 + warnings
    ///（对齐 pdf 后端 Off 模式语义，不报错）。
    pub engine: Option<Arc<dyn OcrEngine>>,
}

/// 注册 djvu 后端（解析能力 + 文档插件）到 `registry`。
pub fn register(registry: &mut Registry, opts: DjVuOptions) {
    let cap: Arc<dyn ParseCapability> = Arc::new(DjvuParseCapability { opts });
    if let Err(e) = registry.register_parse(cap, None) {
        panic!("djvu 后端注册失败：{e}");
    }
    registry.register_plugin(Arc::new(DjvuPlugin));
}

// ---------------------------------------------------------------------------
// 解析能力：文件 → RawDoc（页级分治全流程）
// ---------------------------------------------------------------------------

struct DjvuParseCapability {
    opts: DjVuOptions,
}

impl ParseCapability for DjvuParseCapability {
    fn id(&self) -> &str {
        "djvu-rs"
    }

    fn formats(&self) -> &[&str] {
        &["djvu"]
    }

    fn parse(&self, path: &Path) -> Result<RawDoc> {
        parse_djvu(path, &self.opts)
    }
}

/// 页级分治主流程。
fn parse_djvu(path: &Path, opts: &DjVuOptions) -> Result<RawDoc> {
    // 预检走终止码 Unsupported：文件不存在是确定性结论，不能被兜底后端掩盖
    if !path.is_file() {
        return Err(DocError {
            code: ErrorCode::Unsupported,
            message: format!("文件不存在：{}", path.display()),
            attempts: vec![],
        });
    }

    let doc = DjvuDocument::open(path).map_err(|e| DocError {
        code: ErrorCode::Corrupt,
        message: format!("DjVu 打开失败（{}）：{e}", path.display()),
        attempts: vec![],
    })?;

    let page_count = doc.page_count();
    let mut pages = Vec::with_capacity(page_count);
    let mut textless: Vec<u32> = Vec::new(); // 无字页页号（1 起始）
    for idx in 0..page_count {
        let page = doc.page(idx).map_err(|e| DocError {
            code: ErrorCode::Corrupt,
            message: format!("DjVu 第 {} 页打开失败：{e}", idx + 1),
            attempts: vec![],
        })?;
        let mut raw = RawPage {
            name: Some((idx + 1).to_string()),
            width: Some(page.width() as f32),
            height: Some(page.height() as f32),
            rotation: Some(rotation_degrees(page.rotation())),
            ..Default::default()
        };

        // 快路径：文字层直提（文字层为空白的退化页同样落入渲染分支，
        // B loader L172-180 的 `!text.trim().is_empty()` 同判）
        let items = match page.text_layer().map_err(|e| DocError {
            code: ErrorCode::Corrupt,
            message: format!("DjVu 第 {} 页文字层解析失败：{e}", idx + 1),
            attempts: vec![],
        })? {
            Some(layer) => text_layer_items(&layer),
            None => Vec::new(),
        };

        if !items.is_empty() {
            raw.items = items;
            raw.coord_units = Some(page_coord_units(page.dpi()));
        } else {
            // 慢路径：无字页渲染 + OCR（引擎缺位则空页 + 告警）
            textless.push(idx as u32 + 1);
            if let Some(engine) = opts.engine.as_ref() {
                let img = render_page_rgba(&page, idx + 1)?;
                let lines = engine.recognize(&img).map_err(|e| DocError {
                    code: ErrorCode::Unknown,
                    message: e,
                    attempts: vec![],
                })?;
                raw.items = lines
                    .into_iter()
                    .map(|l| {
                        let bbox = l.axis_rect();
                        RawTextItem {
                            text: l.text,
                            bbox: Some(bbox),
                            quad: Some(l.quad),
                            source: BlockSource::Ocr,
                            confidence: Some(l.score),
                            ..Default::default()
                        }
                    })
                    .collect();
                raw.coord_units = Some(page_coord_units(page.dpi()));
            }
        }
        pages.push(raw);
    }

    let needs_ocr = !textless.is_empty();
    let mut meta = HashMap::new();
    if needs_ocr && opts.engine.is_none() {
        // 无引擎跳过的无字页：页不出块，记告警（Off 语义；有引擎时已识别，
        // 无需告警，needs_ocr 仍为 true 供消费者知晓存在识别文本）
        meta.insert(
            "warnings".to_string(),
            format!(
                "无文字层页未 OCR（未注入 OcrEngine）：第 {} 页",
                textless.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("、")
            ),
        );
    }
    Ok(RawDoc { pages, meta, needs_ocr })
}

// ---------------------------------------------------------------------------
// 文字层 → 行级 items
// ---------------------------------------------------------------------------

/// 文字层 → 行级 `RawTextItem`（TextLayer 来源）。
///
/// 首选 **Line zone**（行框 = djvu-rs 已换算好的左上原点像素 rect）；无 Line
/// 层的退化层级回退 **Para zone**；两者皆无（裸文本 / 仅 Page 根）时整页
/// 文本进一个无 bbox 项。行序 = zone 树文档序（djvused 写入序，一般为
/// 阅读序）。zone 文本可能携带层级分隔符（`\n` 行界与 `\0`/`\x0b`/`\x1d`/
/// `\x1f` 段/区/栏/页界，见 djvu-rs reflowable_text 的分隔符约定），按其
/// trim 后为空的项丢弃。confidence 恒 None（文字层零识别，语义与 pdf 后端
/// 文字层行一致）。
fn text_layer_items(layer: &TextLayer) -> Vec<RawTextItem> {
    let mut lines: Vec<&TextZone> = Vec::new();
    let mut paras: Vec<&TextZone> = Vec::new();
    collect_zones(&layer.zones, &mut lines, &mut paras);
    let zones: &[&TextZone] = if lines.is_empty() { &paras } else { &lines };
    let mut items: Vec<RawTextItem> = zones.iter().filter_map(|z| zone_item(z)).collect();
    if items.is_empty() && !layer.text.trim().is_empty() {
        items.push(RawTextItem {
            text: layer.text.trim().to_string(),
            bbox: None,
            source: BlockSource::TextLayer,
            ..Default::default()
        });
    }
    items
}

/// 先序遍历 zone 树，按类别收集 Line / Para 节点（保文档序）。
fn collect_zones<'a>(zones: &'a [TextZone], lines: &mut Vec<&'a TextZone>, paras: &mut Vec<&'a TextZone>) {
    for z in zones {
        match z.kind {
            TextZoneKind::Line => lines.push(z),
            TextZoneKind::Para => paras.push(z),
            _ => {}
        }
        collect_zones(&z.children, lines, paras);
    }
}

/// 单个 zone → item（文本按层级分隔符 trim，空文本丢弃）。
fn zone_item(zone: &TextZone) -> Option<RawTextItem> {
    const SEPS: [char; 5] = ['\n', '\0', '\u{b}', '\u{1d}', '\u{1f}'];
    let text = zone.text.trim_matches(|c: char| c.is_whitespace() || SEPS.contains(&c));
    if text.is_empty() {
        return None;
    }
    let r = &zone.rect;
    Some(RawTextItem {
        text: text.to_string(),
        bbox: Some(Rect {
            x0: r.x as f32,
            y0: r.y as f32,
            x1: (r.x + r.width) as f32,
            y1: (r.y + r.height) as f32,
        }),
        source: BlockSource::TextLayer,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// 无字页：渲染 + OCR
// ---------------------------------------------------------------------------

/// 无字页渲染为 RGBA（原生分辨率 scale=1、显示方向、左上原点）。
///
/// 与 B loader 的 `render_to_size(800, 1200)` 缩小渲染不同：docforge 的渲染
/// 是 **OCR 输入**，DjVu 页本身即位图（无矢量放大收益），保持原生分辨率
/// 才不损失识别精度。djvu-rs `Pixmap` 的 data 是 RGBA 行主序字节，与
/// image 0.25 的 `RgbaImage::from_raw` 布局一致，零拷贝换装。
fn render_page_rgba(page: &djvu_rs::Page<'_>, page_no: usize) -> Result<image::RgbaImage> {
    let pm = page.render().map_err(|e| DocError {
        code: ErrorCode::Corrupt,
        message: format!("DjVu 第 {page_no} 页渲染失败：{e}"),
        attempts: vec![],
    })?;
    image::RgbaImage::from_raw(pm.width, pm.height, pm.data).ok_or_else(|| DocError {
        code: ErrorCode::Corrupt,
        message: format!("DjVu 第 {page_no} 页渲染像素数据长度异常"),
        attempts: vec![],
    })
}

/// 页几何的坐标单位：INFO dpi ≥1 → `Px{dpi}`；0（病态/手工件）→ `Logical`。
fn page_coord_units(dpi: u16) -> CoordUnits {
    if dpi == 0 {
        CoordUnits::Logical
    } else {
        CoordUnits::Px { dpi: dpi as f32 }
    }
}

/// INFO 旋转角 → 度（IR `Page.rotation` 语义）。
fn rotation_degrees(r: Rotation) -> i32 {
    match r {
        Rotation::None => 0,
        Rotation::Cw90 => 90,
        Rotation::Rot180 => 180,
        Rotation::Ccw90 => 270,
    }
}

// ---------------------------------------------------------------------------
// 文档插件：RawDoc → IR（行块透传 OCR 语义字段，形态照抄 PdfPlugin）
// ---------------------------------------------------------------------------

struct DjvuPlugin;

impl FormatPlugin for DjvuPlugin {
    fn id(&self) -> &str {
        "djvu"
    }

    fn formats(&self) -> &[&str] {
        &["djvu"]
    }

    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["djvu-rs"]
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
// 测试样例生成（djvu-rs 自带编码器：Bitmap + PageEncoder(TXTz) + djvm::merge，
// 无需外部 .djvu 件——两 vendor 目录与 crates.io 存档均不含语料件）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod sample {
    use super::*;

    pub const PAGE_W: u32 = 1000;
    pub const PAGE_H: u32 = 1400;
    pub const PAGE_DPI: u16 = 300;

    /// 三行样例文本：(文本, 行框 x, 行框 y)——行框 600×40，左上原点像素。
    pub const SAMPLE_LINES: [(&str, u32, u32); 3] = [
        ("深秋的校园格外安静，银杏叶铺满林荫道。", 60, 100),
        ("图书馆的阅览室里，学生们埋头写着报告。", 60, 160),
        ("docforge djvu text-layer 2026 第 3 行", 60, 220),
    ];

    /// OCR mock 的固定识别行（quad 落在页 1 条带区，左上原点像素）。
    pub const MOCK_TEXT: &str = "扫描页识别出的文本行";
    pub const MOCK_SCORE: f32 = 0.87;

    fn line_rect(x: u32, y: u32) -> djvu_rs::text::Rect {
        djvu_rs::text::Rect { x, y, width: 600, height: 40 }
    }

    /// 三行样例的文字层：Page → Para → Line×3（djvused 常见形态；
    /// 全文以 `\n` 行界连接，zone.text 必须是其精确子串——编码器按子串
    /// 定位 text_start，text_encode.rs L75-90）。
    pub fn sample_layer() -> TextLayer {
        let text = SAMPLE_LINES.iter().map(|(t, _, _)| *t).collect::<Vec<_>>().join("\n");
        let line_zones = SAMPLE_LINES
            .iter()
            .map(|(t, x, y)| TextZone {
                kind: TextZoneKind::Line,
                rect: line_rect(*x, *y),
                text: (*t).to_string(),
                children: vec![],
            })
            .collect();
        TextLayer {
            text: text.clone(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: djvu_rs::text::Rect { x: 0, y: 0, width: PAGE_W, height: PAGE_H },
                text: text.clone(),
                children: vec![TextZone {
                    kind: TextZoneKind::Para,
                    rect: djvu_rs::text::Rect { x: 40, y: 80, width: 900, height: 200 },
                    text,
                    children: line_zones,
                }],
            }],
        }
    }

    /// 仅 Para 层（无 Line）的退化文字层：Para 回退路径用。
    pub fn para_only_layer() -> TextLayer {
        let text = "整段文本没有行级 zone，回退到段落项。".to_string();
        TextLayer {
            text: text.clone(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: djvu_rs::text::Rect { x: 0, y: 0, width: PAGE_W, height: PAGE_H },
                text: text.clone(),
                children: vec![TextZone {
                    kind: TextZoneKind::Para,
                    rect: djvu_rs::text::Rect { x: 50, y: 90, width: 800, height: 120 },
                    text,
                    children: vec![],
                }],
            }],
        }
    }

    /// 裸文本层（无任何 zone）：单条无 bbox 项路径用。
    pub fn flat_layer() -> TextLayer {
        TextLayer { text: "无 zone 的裸文本层".to_string(), zones: vec![] }
    }

    /// 编码单页 .djvu：白底 + 可选黑色条带（模拟可见文字），可选挂文字层
    ///（TXTz，BZZ 压缩——真实扫描书的主流形态）。
    pub fn page_bytes(layer: Option<TextLayer>, stripes: bool) -> Vec<u8> {
        let mut bm = djvu_rs::Bitmap::new(PAGE_W, PAGE_H);
        if stripes {
            // 三条黑带，位置与 SAMPLE_LINES 行框一致（渲染件有真实内容）
            for (_, x, y) in SAMPLE_LINES {
                for py in y..y + 40 {
                    for px in x..x + 600 {
                        bm.set_black(px, py);
                    }
                }
            }
        }
        let mut enc = djvu_rs::djvu_encode::PageEncoder::from_bitmap(&bm).with_dpi(PAGE_DPI);
        if let Some(layer) = layer {
            enc = enc.with_text_layer(layer);
        }
        enc.encode().expect("样例页编码失败")
    }

    /// 双页件：页 1 带文字层 + 条带，页 2 无文字层 + 条带（扫描页）。
    pub fn mixed_two_page() -> Vec<u8> {
        let p1 = page_bytes(Some(sample_layer()), true);
        let p2 = page_bytes(None, true);
        djvu_rs::djvm::merge(&[&p1, &p2]).expect("样例合并失败")
    }
}

// ---------------------------------------------------------------------------
// 金测用例
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::sample;
    use super::*;
    use crate::model::BlockSource;
    use crate::ocr::RecognizedLine;

    /// 样例输出统一放 target/ 下，目录随进程隔离避免并行互踩。
    fn probe_dir(tag: &str) -> std::path::PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("djvu-m4-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_sample(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = probe_dir(tag);
        let path = dir.join(format!("{tag}.djvu"));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// 渲染尺寸校验 + 固定单行输出的 mock 引擎（不依赖模型缓存）。
    struct MockEngine {
        expect_size: (u32, u32),
    }

    impl OcrEngine for MockEngine {
        fn name(&self) -> &str {
            "mock"
        }

        fn recognize(
            &self,
            image: &image::RgbaImage,
        ) -> std::result::Result<Vec<RecognizedLine>, String> {
            if image.dimensions() != self.expect_size {
                return Err(format!(
                    "渲染尺寸 {:?} ≠ 期望 {:?}",
                    image.dimensions(),
                    self.expect_size
                ));
            }
            Ok(vec![RecognizedLine {
                text: sample::MOCK_TEXT.to_string(),
                score: sample::MOCK_SCORE,
                quad: [[12.0, 96.0], [620.0, 98.0], [618.0, 148.0], [10.0, 146.0]],
            }])
        }
    }

    #[test]
    fn text_layer_page_extracts_line_items() {
        let path = write_sample("text", &sample::page_bytes(Some(sample::sample_layer()), true));

        let raw = parse_djvu(&path, &DjVuOptions::default()).expect("解析失败");
        assert_eq!(raw.pages.len(), 1);
        let rp = &raw.pages[0];
        assert_eq!(rp.items.len(), 3, "三行样例应产三个行级 item：{:?}", rp.items);
        for (i, (exp, x, y)) in sample::SAMPLE_LINES.iter().enumerate() {
            let it = &rp.items[i];
            assert_eq!(it.text, *exp, "第 {} 行文本应逐字还原", i + 1);
            assert_eq!(it.source, BlockSource::TextLayer);
            assert!(it.confidence.is_none(), "文字层行不带置信度");
            assert!(it.quad.is_none(), "文字层行不带四点框");
            let b = it.bbox.expect("Line zone 应有 bbox");
            // 编码往返后坐标应精确回到构造值（左上原点像素，dpi=300）
            assert_eq!((b.x0, b.y0, b.x1, b.y1), (*x as f32, *y as f32, (x + 600) as f32, (y + 40) as f32),
                "第 {} 行 bbox 编码往返应无损", i + 1);
        }
        assert_eq!(rp.coord_units, Some(CoordUnits::Px { dpi: sample::PAGE_DPI as f32 }));
        assert_eq!(rp.rotation, Some(0));
        assert_eq!(rp.width, Some(sample::PAGE_W as f32));
        assert!(!raw.needs_ocr, "纯文字层件不应标记 needs_ocr");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn textless_page_without_engine_warns_and_skips() {
        let path = write_sample("noeng", &sample::page_bytes(None, true));

        let raw = parse_djvu(&path, &DjVuOptions::default()).expect("无引擎不应失败");
        assert_eq!(raw.pages.len(), 1);
        assert!(raw.pages[0].items.is_empty(), "无字页不出块");
        assert!(raw.pages[0].coord_units.is_none(), "无几何页不标坐标单位");
        assert!(raw.needs_ocr, "无字页被跳过应标记 needs_ocr");
        let w = raw.meta.get("warnings").expect("应有告警");
        assert!(w.contains("第 1 页") && w.contains("未 OCR"), "实际告警：{w}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn textless_page_with_engine_produces_ocr_blocks() {
        let path = write_sample("ocr", &sample::page_bytes(None, true));
        let opts = DjVuOptions {
            engine: Some(Arc::new(MockEngine { expect_size: (sample::PAGE_W, sample::PAGE_H) })),
        };

        let raw = parse_djvu(&path, &opts).expect("OCR 路径失败");
        let rp = &raw.pages[0];
        assert_eq!(rp.items.len(), 1, "mock 引擎单行输出");
        let it = &rp.items[0];
        assert_eq!(it.text, sample::MOCK_TEXT);
        assert_eq!(it.source, BlockSource::Ocr);
        assert_eq!(it.confidence, Some(sample::MOCK_SCORE), "OCR 行应带置信度");
        let b = it.bbox.expect("OCR 行应有 bbox");
        // quad 并集：x 10..620，y 96..148（左上原点像素，渲染原生分辨率）
        assert_eq!((b.x0, b.y0, b.x1, b.y1), (10.0, 96.0, 620.0, 148.0));
        // quad 直通：mock 四点框原样保留（旋转保真，bbox 是其轴对齐包围盒）
        assert_eq!(
            it.quad,
            Some([[12.0, 96.0], [620.0, 98.0], [618.0, 148.0], [10.0, 146.0]])
        );
        // 渲染恒为原生 scale=1 → dpi = 页 dpi
        assert_eq!(rp.coord_units, Some(CoordUnits::Px { dpi: sample::PAGE_DPI as f32 }));
        assert!(raw.needs_ocr, "存在识别文本页应标记 needs_ocr");
        assert!(!raw.meta.contains_key("warnings"), "已识别不应再有告警");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn mixed_document_routes_pages_independently() {
        let path = write_sample("mixed", &sample::mixed_two_page());
        let opts = DjVuOptions {
            engine: Some(Arc::new(MockEngine { expect_size: (sample::PAGE_W, sample::PAGE_H) })),
        };

        let raw = parse_djvu(&path, &opts).expect("混合件解析失败");
        assert_eq!(raw.pages.len(), 2, "djvm::merge 保页序");
        // 页 1：文字层直提
        assert_eq!(raw.pages[0].items.len(), 3);
        assert!(raw.pages[0].items.iter().all(|i| i.source == BlockSource::TextLayer));
        assert_eq!(raw.pages[0].items[0].text, sample::SAMPLE_LINES[0].0);
        // 页 2：渲染 + OCR
        assert_eq!(raw.pages[1].items.len(), 1);
        assert_eq!(raw.pages[1].items[0].source, BlockSource::Ocr);
        assert_eq!(
            raw.pages[1].coord_units,
            Some(CoordUnits::Px { dpi: sample::PAGE_DPI as f32 })
        );
        assert!(raw.needs_ocr);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn para_only_layer_falls_back_to_para_items() {
        let path = write_sample("para", &sample::page_bytes(Some(sample::para_only_layer()), false));

        let raw = parse_djvu(&path, &DjVuOptions::default()).expect("解析失败");
        let rp = &raw.pages[0];
        assert_eq!(rp.items.len(), 1, "无 Line 层时 Para zone 成一个 item");
        assert_eq!(rp.items[0].text, "整段文本没有行级 zone，回退到段落项。");
        assert_eq!(rp.items[0].source, BlockSource::TextLayer);
        assert_eq!(
            rp.items[0].bbox.map(|b| (b.x0, b.y0)),
            Some((50.0, 90.0)),
            "Para 回退沿用段落框"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn flat_text_layer_without_zones_single_item() {
        let path = write_sample("flat", &sample::page_bytes(Some(sample::flat_layer()), false));

        let raw = parse_djvu(&path, &DjVuOptions::default()).expect("解析失败");
        let rp = &raw.pages[0];
        assert_eq!(rp.items.len(), 1, "裸文本层整页进一个 item");
        assert_eq!(rp.items[0].text, "无 zone 的裸文本层");
        assert!(rp.items[0].bbox.is_none(), "无 zone 即无几何");
        assert!(!raw.needs_ocr, "文字层存在（即使无 zone）不算无字页");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_file_error_is_unsupported() {
        let err = parse_djvu(
            Path::new("/nonexistent/docforge/definitely-missing.djvu"),
            &DjVuOptions::default(),
        )
        .expect_err("缺文件应报错");
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert!(err.message.contains("文件不存在"), "实际：{}", err.message);
    }

    #[test]
    fn assemble_maps_items_to_ir_blocks() {
        let path = write_sample("ir", &sample::mixed_two_page());
        let opts = DjVuOptions {
            engine: Some(Arc::new(MockEngine { expect_size: (sample::PAGE_W, sample::PAGE_H) })),
        };

        let raw = parse_djvu(&path, &opts).expect("解析失败");
        let doc = DjvuPlugin.assemble(&raw);
        assert_eq!(doc.pages.len(), 2);

        let p1 = &doc.pages[0];
        assert_eq!(p1.blocks.len(), 3);
        assert!(p1.blocks.iter().all(|b| b.kind == BlockKind::Paragraph));
        assert!(p1.blocks.iter().all(|b| b.source == BlockSource::TextLayer));
        assert!(p1.blocks.iter().all(|b| b.confidence.is_none()));
        assert!(p1.blocks[0].bbox.is_some(), "bbox 透传至 Block");
        assert_eq!(p1.coord_units, Some(CoordUnits::Px { dpi: sample::PAGE_DPI as f32 }));
        assert!(p1.text.contains("深秋的校园"), "页文本应含行文本：{}", p1.text);

        let p2 = &doc.pages[1];
        assert_eq!(p2.blocks.len(), 1);
        assert_eq!(p2.blocks[0].source, BlockSource::Ocr);
        assert_eq!(p2.blocks[0].confidence, Some(sample::MOCK_SCORE));
        assert_eq!(
            p2.blocks[0].quad,
            Some([[12.0, 96.0], [620.0, 98.0], [618.0, 148.0], [10.0, 146.0]]),
            "assemble 透传 quad 至 Block"
        );
        assert_eq!(p2.text, sample::MOCK_TEXT);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
