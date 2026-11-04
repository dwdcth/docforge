//! 跨页重复块检测（running 元素事实标记，id = "running-detect"；零新依赖恒开）。
//!
//! 真实 PDF/DjVu 的痛点：每页重复的页眉（文档名）/页脚（页码）被当作正文
//! 送翻 N 遍、译文附录被灌满。本模块在解析侧（处理链）检测「同文本每页
//! 同位置」的跨页重复块，就地标记 `RawTextItem.meta["running"] =
//! Bool(true)`，assemble 透传为 `Block.meta`，最终由
//! [`crate::emit::to_translation_units`] 复制为 `TranslationUnit.running`。
//! pdf / djvu 的默认处理链已含本能力（有几何的多页格式）；其余格式不加，
//! `should_apply` 的页数闸是第二道自然跳过。
//!
//! # 只标事实，策略归消费者
//!
//! 与 [`crate::emit::has_translatable_text`] 同一分层原则：`running = true`
//! 只是**事实**——这段文本以近同位置出现在 ≥ [`MIN_REPEAT_PAGES`] 个不同页
//! 上（页眉/页脚/重复段首的典型形态）。消费语义归消费者：跳过送译、或按
//! 文本键合并翻一次，都是下游策略；**单元照发**（锚点契约不动）。附录侧
//! 的默认跳过是库定的唯一默认行为（`writeback::appendix`）。
//!
//! # 与 docx regions→Note 块机制的区分
//!
//! docx 的页眉/页脚走 `RawPage.regions` → `BlockKind::Note` 块
//! （`meta["region"]` 标注**内容来源区域**类型）——那是结构事实，另一机制。
//! 本模块的 `meta["region"]`（`"header"` / `"footer"`）是打在**任意普通块**
//! 上的位置带事实（bbox 视觉中心在页顶 25% / 页底 25%），与 Note 块的
//! region 语义同名不同义；`running` 键同样在 Note 块语义之外——Note 块
//! （docx 页眉）不进页 items、不参与跨页计数，二者不会互相污染。
//!
//! # 判定算法（两遍扫描）
//!
//! 第一遍按 **(位置键, 文本键)** 分组，统计组覆盖的**不同页数**；第二遍对
//! 覆盖页数 ≥ [`MIN_REPEAT_PAGES`] 的组内条目标 `running`（同页出现多次只
//! 计一页——同页重复是行拆分伪影，不是跨页 running）。
//!
//! - **文本键**：trim + 连续空白折叠为单空格 + 连续 ASCII 数字串归一为
//!   `#`——页码变体（"第 3 页" / "第 4 页" / "Page 3"）归同键。逐字匹配
//!   抓不到变体页码，这是痛点核心。
//! - **位置键（两口径，分键空间不互串）**：
//!   - 页有 w/h 时**归一化**：bbox `(x0/w, y, x1/w, y)` 按**各页自身 w/h**
//!     归一（单位无关），各分量四舍五入量化到 1/50 网格（约 ±2% 容差，
//!     吸收跨页提取抖动）。y 取**视觉坐标**（自页顶起量，小者在前）：
//!     pdf 文字层页（`CoordUnits::Pt`，左下原点 y 向上）翻转 `1 - y/h`，
//!     与 OCR 页 / DjVu（左上原点 y 向下，契约见各后端模块头）天然同向
//!     ——同一条页眉在两类页上的归一位置一致，可跨路由合并。
//!   - 页**无 w/h** 时**原始坐标**：pdf 文字层页常态（pdf-inspector 公开
//!     API 不暴露页框、页不出尺寸，见 pdf 后端 `produce_page`——若此口径
//!     也按「无 w/h 不参与」跳过，特性对文字层 PDF 这一头号痛点直接失效）。
//!     同一 PDF 的页框恒同、坐标系一致，原始点坐标在同路由内天然可比，
//!     按固定 12pt 网格量化（≈ 2% A4 宽，与归一化口径容差同量级）。两口径
//!     键空间加标签区分，杜绝数值域意外碰撞。
//! - **判定**：组覆盖的不同页数 ≥ [`MIN_REPEAT_PAGES`]（3）。2 页重复可能
//!   是巧合（正文小节在两页各现一次），宁漏勿错：漏标只多翻一遍，错标
//!   会把正文当页眉丢掉。
//! - **位置带细标（仅归一化口径）**：条目自身 bbox 的视觉中心 y < 0.25 →
//!   `meta["region"] = "header"`；> 0.75 → `"footer"`；中间不标（running
//!   事实仍在）。原始坐标口径页高未知——带不是可靠事实，只标 running
//!   不标 region（只标事实，不猜测）。
//! - **跳过**：无 bbox 的条目不参与（不贡献计数也不被标）；总页数
//!   < [`MIN_REPEAT_PAGES`] 时 `should_apply` 直接 false。

use std::collections::{HashMap, HashSet};

use crate::error::Result;
use crate::model::{CoordUnits, MetaValue};
use crate::plugin::{ProcessCapability, RawDoc, RawPage, RawTextItem};

/// 判定阈值：组覆盖的不同页数 ≥ 3 才标 running（2 页重复可能是巧合正文）。
const MIN_REPEAT_PAGES: usize = 3;

/// 归一化位置量化网格：分量四舍五入到 1/50（约 ±2% 容差）。
const POS_GRID: f32 = 50.0;
/// 原始坐标量化网格（pt）：无 w/h 页的键口径。12pt ≈ 2% A4 宽，与归一化
/// 口径的容差同量级。
const RAW_GRID: f32 = 12.0;

/// 位置带阈值（视觉归一 y）：中心 < 此值 → 页眉带。
const HEADER_BAND: f32 = 0.25;
/// 位置带阈值（视觉归一 y）：中心 > 此值 → 页脚带。
const FOOTER_BAND: f32 = 0.75;

/// 跨页重复块检测处理能力。
pub struct RunningDetect;

/// 位置键空间：归一化（页有 w/h）与原始坐标（无 w/h 的 pdf 文字层页）
/// 各自成键、互不合并——数值域不同，加标签杜绝意外碰撞。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum KeySpace {
    /// 归一化视觉坐标（页有 w/h）。
    Normalized,
    /// 原始点坐标直量化（页无 w/h）。
    Raw,
}

/// 位置键：键空间 + 四分量量化值（整数元组可 Eq/Hash）。
type PosKey = (KeySpace, [i64; 4]);
/// 组键：(位置键, 文本键)。
type GroupKey = (PosKey, String);

/// 页几何口径：归一化分母（有 w/h，y 轴方向随 coord_units）或原始坐标。
struct PageGeom {
    /// `Some((w, h))` = 归一化口径；`None` = 原始坐标口径。
    dims: Option<(f32, f32)>,
    /// true = PDF 文字层页（`CoordUnits::Pt`，左下原点 y 向上）：视觉
    /// y = 1 - y/h。其余（Px / Logical）坐标已自页顶起量，不翻转。
    y_up: bool,
}

/// 页几何口径判定。w/h 缺失或 ≤ 0 都按原始坐标口径（同文档页框恒同，
/// 原始键在同路由内可比——文字层 PDF 的痛点路径不因页不出尺寸而失效）。
fn page_geom(page: &RawPage) -> PageGeom {
    PageGeom {
        dims: match (page.width, page.height) {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some((w, h)),
            _ => None,
        },
        y_up: page.coord_units == Some(CoordUnits::Pt),
    }
}

impl PageGeom {
    /// 条目的 (位置键, 视觉中心 y)。无 bbox → None（不参与检测）；原始
    /// 坐标口径中心为 None（页高未知，位置带不可判）。
    fn key_of(&self, item: &RawTextItem) -> Option<(PosKey, Option<f32>)> {
        let b = item.bbox.as_ref()?;
        let q = |v: f32, grid: f32| (v * grid).round() as i64;
        match self.dims {
            Some((w, h)) => {
                let vis_y = |y: f32| {
                    let n = y / h;
                    if self.y_up { 1.0 - n } else { n }
                };
                let vy0 = vis_y(b.y0);
                let vy1 = vis_y(b.y1);
                // y 两分量按视觉序（小者在前）入键——Pt 页翻转后 raw 的
                // y0/y1 视觉序对调，不排序则同一条带的键在 y-up / y-down
                // 两系不可比。
                let (ylo, yhi) = (vy0.min(vy1), vy0.max(vy1));
                Some((
                    (
                        KeySpace::Normalized,
                        [
                            q(b.x0 / w, POS_GRID),
                            q(ylo, POS_GRID),
                            q(b.x1 / w, POS_GRID),
                            q(yhi, POS_GRID),
                        ],
                    ),
                    Some((ylo + yhi) / 2.0),
                ))
            }
            None => {
                let (ylo, yhi) = (b.y0.min(b.y1), b.y0.max(b.y1));
                Some((
                    (
                        KeySpace::Raw,
                        [
                            q(b.x0, RAW_GRID),
                            q(ylo, RAW_GRID),
                            q(b.x1, RAW_GRID),
                            q(yhi, RAW_GRID),
                        ],
                    ),
                    None,
                ))
            }
        }
    }
}

/// 文本键：trim + 连续空白折叠为单空格 + 连续 ASCII 数字串归一为 `#`。
fn text_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_ws = true; // 视开头为空白：前导空白不产出
    let mut in_digits = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
            prev_ws = false;
        } else {
            in_digits = false;
            if c.is_whitespace() {
                if !prev_ws {
                    out.push(' ');
                    prev_ws = true;
                }
            } else {
                out.push(c);
                prev_ws = false;
            }
        }
    }
    if out.ends_with(' ') {
        out.pop(); // 尾部空白 = trim
    }
    out
}

impl ProcessCapability for RunningDetect {
    fn id(&self) -> &str {
        "running-detect"
    }

    fn should_apply(&self, raw: &RawDoc) -> bool {
        raw.pages.len() >= MIN_REPEAT_PAGES
    }

    fn process(&self, raw: &mut RawDoc) -> Result<()> {
        // 第一遍：分组计数（组 → 出现过的不同页集合）
        let mut groups: HashMap<GroupKey, HashSet<usize>> = HashMap::new();
        for (p_idx, page) in raw.pages.iter().enumerate() {
            let geom = page_geom(page);
            for item in &page.items {
                let Some((key, _)) = geom.key_of(item) else { continue };
                let text = text_key(&item.text);
                groups.entry((key, text)).or_default().insert(p_idx);
            }
        }
        // 命中组集合：覆盖不同页数达阈值
        let marked: HashSet<GroupKey> = groups
            .into_iter()
            .filter(|(_, pages)| pages.len() >= MIN_REPEAT_PAGES)
            .map(|(k, _)| k)
            .collect();
        if marked.is_empty() {
            return Ok(());
        }
        // 第二遍：命中组内的条目标 running + 位置带 region（条目自身中心定
        // 带；仅归一化口径有中心——原始口径页高未知，不猜带）
        for page in &mut raw.pages {
            let geom = page_geom(page);
            for item in &mut page.items {
                let Some((key, center)) = geom.key_of(item) else { continue };
                let text = text_key(&item.text);
                if !marked.contains(&(key, text)) {
                    continue;
                }
                item.meta.insert("running".to_string(), MetaValue::Bool(true));
                if let Some(cy) = center {
                    let region = if cy < HEADER_BAND {
                        Some("header")
                    } else if cy > FOOTER_BAND {
                        Some("footer")
                    } else {
                        None
                    };
                    if let Some(r) = region {
                        item.meta.insert("region".to_string(), MetaValue::Str(r.into()));
                    }
                }
            }
        }
        Ok(())
    }
}

/// 注册到 Registry 的处理能力位（配合插件 default_process_chain 的
/// "running-detect" 条目生效；pdf / djvu 默认链已含）。
pub fn register(reg: &mut crate::registry::Registry) {
    use std::sync::Arc;
    reg.register_process(Arc::new(RunningDetect));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Rect;
    use crate::plugin::RawPage;

    /// 测试页几何：像素页（左上原点，同 OCR 页语义）1000×1400。
    const W: f32 = 1000.0;
    const H: f32 = 1400.0;

    /// 带几何的像素页。
    fn px_page(items: Vec<RawTextItem>) -> RawPage {
        RawPage {
            items,
            width: Some(W),
            height: Some(H),
            coord_units: Some(CoordUnits::Px { dpi: 200.0 }),
            ..Default::default()
        }
    }

    /// 无尺寸页（pdf 文字层页形态：Pt 坐标、页不出 w/h → 原始坐标键）。
    fn pt_page(items: Vec<RawTextItem>) -> RawPage {
        RawPage {
            items,
            coord_units: Some(CoordUnits::Pt),
            ..Default::default()
        }
    }

    /// 带框条目。
    fn item(text: &str, bbox: Rect) -> RawTextItem {
        RawTextItem {
            text: text.into(),
            bbox: Some(bbox),
            ..Default::default()
        }
    }

    /// 视觉页顶条带（中心 y ≈ 0.043 < 0.25 → header 带）。
    fn header_box() -> Rect {
        Rect { x0: 40.0, y0: 40.0, x1: 700.0, y1: 80.0 }
    }

    /// 视觉页底条带（中心 y ≈ 0.957 > 0.75 → footer 带）。
    fn footer_box(x_jitter: f32) -> Rect {
        Rect { x0: 40.0 + x_jitter, y0: 1320.0, x1: 300.0 + x_jitter, y1: 1360.0 }
    }

    /// 页中部条带（中心 y ≈ 0.43 → 无位置带）。
    fn mid_box() -> Rect {
        Rect { x0: 60.0, y0: 560.0, x1: 900.0, y1: 640.0 }
    }

    /// 各页互不相同的正文句（文本键各异——数字归一会把「第 N 页…」类
    /// 编号句折成同键，正文必须整体不同才不误标）。
    const BODIES: [&str; 6] = [
        "深秋的校园格外安静",
        "图书馆里灯火通明",
        "会议室的灯亮到深夜",
        "他推开门把文件放下",
        "方案改到第七稿才定",
        "晨雾还没漫过山谷",
    ];

    fn running_of(raw: &RawDoc, p: usize, i: usize) -> bool {
        matches!(
            raw.pages[p].items[i].meta.get("running"),
            Some(MetaValue::Bool(true))
        )
    }

    fn region_of(raw: &RawDoc, p: usize, i: usize) -> Option<&str> {
        match raw.pages[p].items[i].meta.get("region") {
            Some(MetaValue::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    /// 5 页同位页眉 + "第 N 页"页脚（数字归一命中）+ 各页唯一正文：
    /// 页眉/页脚项标 running + 对应 region，正文不标。页 2 的页脚 x 抖
    /// 1.5px（< 2% 容差）仍在同组——量化容差生效。
    #[test]
    fn header_footer_marked_body_not() {
        let mut raw = RawDoc::default();
        for (i, body) in BODIES.iter().enumerate().take(5) {
            raw.pages.push(px_page(vec![
                item("季度运营报告", header_box()),
                item(body, mid_box()),
                item(&format!("第 {} 页", i + 1), footer_box(if i == 2 { 1.5 } else { 0.0 })),
            ]));
        }
        assert!(RunningDetect.should_apply(&raw));
        RunningDetect.process(&mut raw).unwrap();
        for p in 0..5 {
            assert!(running_of(&raw, p, 0), "第 {p} 页页眉应标 running");
            assert_eq!(region_of(&raw, p, 0), Some("header"));
            assert!(!running_of(&raw, p, 1), "第 {p} 页正文不应标");
            assert_eq!(region_of(&raw, p, 1), None);
            assert!(running_of(&raw, p, 2), "第 {p} 页页脚应标 running（数字归一命中）");
            assert_eq!(region_of(&raw, p, 2), Some("footer"));
        }
    }

    /// 2 页重复不达标不标（阈值 3：宁漏勿错）。
    #[test]
    fn two_page_repeat_not_marked() {
        let mut raw = RawDoc::default();
        for (i, body) in BODIES.iter().enumerate().take(4) {
            let mut items = Vec::new();
            if i < 2 {
                items.push(item("两页才见的口号", header_box()));
            }
            items.push(item(body, mid_box()));
            raw.pages.push(px_page(items));
        }
        RunningDetect.process(&mut raw).unwrap();
        for p in 0..4 {
            assert!(!running_of(&raw, p, 0), "第 {p} 页：口号只现于 2 页，不达标");
        }
    }

    /// 同页重复只计一页：同一文本同页 5 次是行拆分伪影，组覆盖 1 页，
    /// 不标（不同页位置的同文本才构成 running）。
    #[test]
    fn same_page_repeat_not_marked() {
        let mut raw = RawDoc::default();
        raw.pages.push(px_page(vec![
            item("同页反复横跳", header_box()),
            item("同页反复横跳", mid_box()),
            item("同页反复横跳", footer_box(0.0)),
            item("同页反复横跳", Rect { x0: 60.0, y0: 900.0, x1: 400.0, y1: 940.0 }),
            item("同页反复横跳", Rect { x0: 60.0, y0: 1000.0, x1: 400.0, y1: 1040.0 }),
        ]));
        raw.pages.push(px_page(vec![item(BODIES[0], mid_box())]));
        raw.pages.push(px_page(vec![item(BODIES[1], mid_box())]));
        RunningDetect.process(&mut raw).unwrap();
        assert!(
            raw.pages[0].items.iter().all(|i| !i.meta.contains_key("running")),
            "同页 5 次只计 1 页，不达标"
        );
    }

    /// 无 bbox 的条目不参与：不贡献组计数、也不被标。页眉 5 页达标作
    /// 正向对照，同页的无框重复文本永不标。
    #[test]
    fn no_bbox_skipped() {
        let mut raw = RawDoc::default();
        for body in BODIES.iter() {
            let mut page = px_page(vec![
                item(body, mid_box()),
                item("全员页眉", header_box()),
            ]);
            page.items.push(RawTextItem { text: "没有框的文本".into(), ..Default::default() });
            raw.pages.push(page);
        }
        RunningDetect.process(&mut raw).unwrap();
        for p in 0..5 {
            assert!(running_of(&raw, p, 1), "页眉 5 页达标（正向对照）");
            assert!(!running_of(&raw, p, 2), "无 bbox 条目不参与也不标");
        }
    }

    /// 页中部跨页重复只标 running、不加位置带。
    #[test]
    fn mid_page_repeat_running_no_region() {
        let mut raw = RawDoc::default();
        let band = Rect { x0: 60.0, y0: 700.0, x1: 800.0, y1: 740.0 };
        for body in &BODIES[..3] {
            raw.pages.push(px_page(vec![item(body, mid_box()), item("每页中部都有的句子", band)]));
        }
        RunningDetect.process(&mut raw).unwrap();
        for p in 0..3 {
            assert!(running_of(&raw, p, 1), "页中部重复标 running");
            assert_eq!(region_of(&raw, p, 1), None, "中部不给位置带");
            assert!(!running_of(&raw, p, 0));
        }
    }

    /// 无 w/h 页（pdf 文字层页形态）走原始坐标键：同位页眉/页脚照标
    /// running，但页高未知 → 不带位置带（只标事实，不猜测）。
    /// 页码数字归一在 raw 口径同样命中。
    #[test]
    fn no_dims_pages_mark_running_without_region() {
        let mut raw = RawDoc::default();
        for (i, body) in BODIES.iter().enumerate().take(5) {
            raw.pages.push(pt_page(vec![
                item("季度运营报告", Rect { x0: 60.0, y0: 780.0, x1: 500.0, y1: 800.0 }),
                item(body, Rect { x0: 60.0, y0: 400.0, x1: 500.0, y1: 420.0 }),
                item(&format!("第 {} 页", i + 1), Rect { x0: 60.0, y0: 40.0, x1: 200.0, y1: 60.0 }),
            ]));
        }
        RunningDetect.process(&mut raw).unwrap();
        for p in 0..5 {
            assert!(running_of(&raw, p, 0), "第 {p} 页页眉（raw 键）应标 running");
            assert_eq!(region_of(&raw, p, 0), None, "页高未知：不带位置带");
            assert!(!running_of(&raw, p, 1));
            assert!(running_of(&raw, p, 2), "页码数字归一命中");
            assert_eq!(region_of(&raw, p, 2), None);
        }
    }

    /// 两口径不互串：同一条页眉在 3 页无尺寸页（raw 键）+ 2 页有尺寸页
    ///（归一化键）上 → raw 组 3 页达标（只标这 3 页、无 region），
    /// 归一化组 2 页不达标。
    #[test]
    fn raw_and_normalized_keys_do_not_merge() {
        let mut raw = RawDoc::default();
        for body in &BODIES[..3] {
            raw.pages.push(pt_page(vec![
                item("跨口径页眉", Rect { x0: 60.0, y0: 780.0, x1: 500.0, y1: 800.0 }),
                item(body, Rect { x0: 60.0, y0: 400.0, x1: 500.0, y1: 420.0 }),
            ]));
        }
        for body in &BODIES[3..5] {
            raw.pages.push(px_page(vec![
                item("跨口径页眉", header_box()),
                item(body, mid_box()),
            ]));
        }
        RunningDetect.process(&mut raw).unwrap();
        for p in 0..3 {
            assert!(running_of(&raw, p, 0), "raw 组 3 页达标");
            assert_eq!(region_of(&raw, p, 0), None);
        }
        for p in 3..5 {
            assert!(!running_of(&raw, p, 0), "归一化组只 2 页，不达标");
        }
    }

    /// Pt 文字层页**带**几何（M4 页内分治页形态：渲染反推出页尺寸）：
    /// 页顶条目（y≈780-800/842）应标 header 带、页底条目（y≈40-60）标
    /// footer 带——视觉坐标翻转回归（不翻转会把页眉错标成页脚带）。
    #[test]
    fn pt_pages_with_dims_flip_y_for_region() {
        let mut raw = RawDoc::default();
        for (i, body) in BODIES.iter().enumerate().take(3) {
            raw.pages.push(RawPage {
                items: vec![
                    item("PDF 版式说明文档", Rect { x0: 60.0, y0: 780.0, x1: 500.0, y1: 800.0 }),
                    item(body, Rect { x0: 60.0, y0: 400.0, x1: 500.0, y1: 420.0 }),
                    item(&format!("第 {} 页", i + 1), Rect { x0: 60.0, y0: 40.0, x1: 200.0, y1: 60.0 }),
                ],
                width: Some(595.0),
                height: Some(842.0),
                coord_units: Some(CoordUnits::Pt),
                ..Default::default()
            });
        }
        RunningDetect.process(&mut raw).unwrap();
        for p in 0..3 {
            assert!(running_of(&raw, p, 0));
            assert_eq!(region_of(&raw, p, 0), Some("header"), "Pt 页 y 向上：页顶 = 视觉 header");
            assert!(!running_of(&raw, p, 1));
            assert!(running_of(&raw, p, 2), "页码数字归一命中");
            assert_eq!(region_of(&raw, p, 2), Some("footer"));
        }
    }

    /// 混合路由（Pt 文字层页 + Px OCR 页，均有几何）可合并分组：同一条
    /// 页眉（归一化后同处视觉页顶同一格）在 2 页 Pt + 3 页 Px 上 → 覆盖
    /// 5 个不同页 ≥ 3，全部标记、全部 header 带——「Pt/Px 天然可比对」
    /// 的回归锚。
    #[test]
    fn pt_and_px_pages_merge_into_one_group() {
        // 归一化目标带 (0.10, 0.06, 0.70, 0.10)：两系量化后同键 [5,3,35,5]
        let mk_pt = |body: &str| RawPage {
            items: vec![
                item("混合路由样例文档", Rect { x0: 59.5, y0: 757.8, x1: 416.5, y1: 791.48 }),
                item(body, Rect { x0: 60.0, y0: 400.0, x1: 500.0, y1: 420.0 }),
            ],
            width: Some(595.0),
            height: Some(842.0),
            coord_units: Some(CoordUnits::Pt),
            ..Default::default()
        };
        let mk_px = |body: &str| RawPage {
            items: vec![
                item("混合路由样例文档", Rect { x0: 100.0, y0: 84.0, x1: 700.0, y1: 140.0 }),
                item(body, Rect { x0: 100.0, y0: 600.0, x1: 840.0, y1: 640.0 }),
            ],
            width: Some(W),
            height: Some(H),
            coord_units: Some(CoordUnits::Px { dpi: 200.0 }),
            ..Default::default()
        };
        let mut raw = RawDoc {
            pages: vec![
                mk_pt(BODIES[0]),
                mk_px(BODIES[1]),
                mk_pt(BODIES[2]),
                mk_px(BODIES[3]),
                mk_px(BODIES[4]),
            ],
            ..Default::default()
        };
        RunningDetect.process(&mut raw).unwrap();
        for p in 0..5 {
            assert!(running_of(&raw, p, 0), "第 {p} 页页眉跨路由命中");
            assert_eq!(region_of(&raw, p, 0), Some("header"), "Pt/Px 归一后同处视觉页顶");
            assert!(!running_of(&raw, p, 1), "正文各页唯一不标");
        }
    }

    /// 页数闸：总页数 < 3 时 should_apply 直接 false。
    #[test]
    fn should_apply_needs_three_pages() {
        let mk = || px_page(vec![item("任意正文", mid_box())]);
        let raw0 = RawDoc::default();
        assert!(!RunningDetect.should_apply(&raw0));
        let raw2 = RawDoc { pages: vec![mk(), mk()], ..Default::default() };
        assert!(!RunningDetect.should_apply(&raw2));
        let raw3 = RawDoc { pages: vec![mk(), mk(), mk()], ..Default::default() };
        assert!(RunningDetect.should_apply(&raw3));
    }

    /// 文本键归一口径：数字串归一（页码变体归同键）、空白折叠、trim。
    #[test]
    fn text_key_normalization() {
        assert_eq!(text_key("第 3 页"), "第 # 页");
        assert_eq!(text_key("第 4 页"), text_key("第 3 页"), "页码变体归同键");
        assert_eq!(text_key("Page 3"), text_key("Page 12"), "英文页码同样归一");
        assert_eq!(text_key("  A   B\t\tC  "), "A B C", "空白折叠 + trim");
        assert_eq!(text_key("v2.0 build 100"), "v#.# build #");
        assert_eq!(text_key("   "), "", "纯空白为空键");
        assert_eq!(text_key("无数字文本"), "无数字文本", "无数字原样（折叠后）");
    }
}
