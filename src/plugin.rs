// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① RawTextItem 新增 source/confidence；② RawPage 新增 coord_units
// （M1：pdf 页级路由把 OCR 语义透传到 IR，docforge-spec-m1.md §4）；
// ③ FormatPlugin 新增 enrich_meta 组装后置钩子（格式特有元数据 → DocMeta.extra）；
// ④ RawTextItem 新增 quad（OCR 旋转保真四点框直通，rumi 集成增强）。
//! 能力抽象（与 Python `capability.py` 对齐）：
//! 解析能力（格式绑定）→ RawDoc；处理能力（全局共享，就地增强）。

use std::fmt;
use std::path::Path;

use std::collections::BTreeMap;

use crate::error::Result;
use crate::model::{BlockSource, CoordUnits, MetaValue, Rect, TableData};

/// 解析产出的原始文本项：text + 可选 bbox。
#[derive(Debug, Clone, Default)]
pub struct RawTextItem {
    pub text: String,
    pub bbox: Option<Rect>,
    /// 旋转保真四点框（顺时针自左上）：OCR 行直通 `RecognizedLine.quad`，
    /// 轴对齐文本为 None。`assemble` 透传至 `Block.quad`；`bbox` 恒为轴对齐
    /// 包围盒（quad 的超集补充），坐标语义随 `RawPage.coord_units`。
    pub quad: Option<[[f32; 2]; 4]>,
    /// 块来源（pdf 页级路由标 `TextLayer`/`Ocr`；其余格式缺省 `Native`）。
    /// `assemble` 透传至 `Block.source`。
    pub source: BlockSource,
    /// OCR 置信度（`source = Ocr` 时 Some；文字层/原生结构为 None——
    /// 文字层的语义是"零识别"，恒 1.0 的分数留给 OcrLine 适配层补）。
    pub confidence: Option<f32>,
    /// 标题层级（1–6）：Some 表示这一项是标题（ODF `text:outline-level` 等），
    /// None 表示普通段落。插件 `assemble` 据此产出 `BlockKind::Heading`。
    pub level: Option<u8>,
    /// 列表项标记：`Some(ordered)` 表示这一项是列表（docx `w:numPr` 编号段落），
    /// `ordered` 来自 numbering 定义（`bullet` / `none` 为无序）。插件 `assemble`
    /// 据此产出 `BlockKind::List`。标题优先于列表（两个字段都有值时按标题处理）。
    pub list: Option<bool>,
    /// 表格项：`Some(rows)` 表示这一项是表格（对齐 Python item kind `table`）。
    /// `text` 同时保留展开文本（行内 `,` 连接、行间 `\n`），页文本层用它；
    /// 插件 `assemble` 据此产出 `BlockKind::Table`。表格优先于标题/列表。
    pub table: Option<TableData>,
    /// 图表项（pptx `c:chart` 的图表内文本）：`assemble` 产出 `BlockKind::Chart`。
    /// 优先级：表格 > 图表 > 标题 > 列表 > 段落。
    pub chart: bool,
    /// 图片项（对齐 Python item kind `image`）：`Some` 时这一项**只**产
    /// `BlockKind::Image`（content = `ocr_text` 或空串），`text` 恒为空、不进页文本。
    /// 之所以把图片塞进 item 列表而不是只放 `RawPage.images`：Docx/Pptx 的图片块
    /// 在文档里是**有位置**的（Python 把 `("image", im)` 追加在同段落 item 之后），
    /// 只保留 `images` 会让块顺序无法还原。
    pub image: Option<RawImage>,
    /// 块级元信息（xlsx `merges` / `truncated` 等）：`assemble` 原样搬到 `Block.meta`，
    /// 与 Python 在 page.meta 上挂的同类信息对齐。
    pub meta: BTreeMap<String, MetaValue>,
}

/// 解析产出的原始图片（可携带 OCR 结果）。
#[derive(Debug, Clone, Default)]
pub struct RawImage {
    pub data: Vec<u8>,
    pub bbox: Option<Rect>,
    pub ext: String,
    /// 内嵌位图像素尺寸（宽, 高）；装饰图过滤 < 50px 用（对齐 Python `px_size`）
    pub px: Option<(u32, u32)>,
    /// 嵌入图 OCR 文本（未识别为 None；与 Python `meta["ocr_text"]` 对齐）
    pub ocr_text: Option<String>,
}

/// 页面上的一块「区域」内容（docx 页眉 / 页脚 / 脚注 / 尾注）：
/// 与正文分离，自带类型；默认既不进块也不进页文本。
#[derive(Debug, Clone, Default)]
pub struct RawRegion {
    /// 区域类型：`header` / `footer` / `footnote` / `endnote`
    /// （与 Python `page.meta["regions"]` 的键、以及 `Block.meta["region"]` 的取值一致）
    pub kind: String,
    /// 区域内的一段文本（同一区域可能有多段，各自成块）
    pub text: String,
}

/// 单元格批注（xlsx）：与正文分离，默认既不进块也不进页文本。
#[derive(Debug, Clone, Default)]
pub struct RawComment {
    /// 单元格引用（`A1` / `B3`）：进 `Block.meta["cell"]`，便于下游定位回写
    pub cell: String,
    /// 批注正文（已 trim）
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct RawPage {
    pub name: Option<String>,
    pub items: Vec<RawTextItem>,
    pub images: Vec<RawImage>,
    /// 附属文本（pptx 备注页等）：不进块，只追加到页文本末尾
    /// （对齐 Python `page.meta["notes"]` 的默认行为）
    pub notes: Option<String>,
    /// 区域内容（docx 页眉/页脚/脚注/尾注/批注）：`assemble` 在 `include_regions`
    /// 打开时按 `BlockKind::Note` + `meta["region"] = kind` 产出，**不进页文本**
    /// （对齐 Python `DocxPlugin.assemble`；`kind = "comment"` 是 Rust 侧新增，
    /// 批注作者/日期不进文本）。
    pub regions: Vec<RawRegion>,
    /// 单元格批注（xlsx）：`assemble` 在 `include_regions` 打开时按
    /// `BlockKind::Note` + `meta["region"] = "comment"` 产出，**不进页文本**
    /// （对齐 Python `XlsxPlugin.assemble`）。
    pub comments: Vec<RawComment>,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub rotation: Option<i32>,
    /// 本页 items bbox 的坐标单位（pdf 文字层页 `Pt` / OCR 页 `Px{dpi}`）。
    /// `assemble` 透传至 `Page.coord_units`；无几何格式为 None。
    pub coord_units: Option<CoordUnits>,
    /// 解析即已得到的表格结构 rows × cells
    pub tables: Vec<Vec<Vec<String>>>,
    /// 快路径页文本（`structure_mode` 非语义分支）：页文本直接取此值
    /// （`"\n".join(texts)`）。docforge 刻意偏离 Python 的 `blocks=[]`：
    /// assemble 仍**按行产 Paragraph 块**（零块文档对翻译/RAG 消费者等于
    /// 不可用）。仅 `regions`/`comments` 仍随 `include_regions` 产 `Note`
    /// 块；`notes` 被忽略（Python 非语义分支不看 `meta["notes"]`）。
    pub flat_text: Option<String>,
}

/// 解析能力的输出：尚未经过处理能力。
#[derive(Debug, Clone, Default)]
pub struct RawDoc {
    pub pages: Vec<RawPage>,
    pub meta: std::collections::HashMap<String, String>,
    pub needs_ocr: bool,
}

/// 能力实现形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Bundled,
    External,
    Composite,
}

impl BackendKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendKind::Bundled => "bundled",
            BackendKind::External => "external",
            BackendKind::Composite => "composite",
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 解析能力：格式绑定，把文件读成 RawDoc。
pub trait ParseCapability: Send + Sync {
    fn id(&self) -> &str;
    fn formats(&self) -> &[&str];
    fn kind(&self) -> BackendKind {
        BackendKind::Bundled
    }
    fn is_available(&self) -> bool {
        true
    }
    fn parse(&self, path: &Path) -> Result<RawDoc>;
}

/// 处理能力：全局共享，就地增强 RawDoc（OCR / layout）。
pub trait ProcessCapability: Send + Sync {
    fn id(&self) -> &str;
    fn is_available(&self) -> bool {
        true
    }
    fn should_apply(&self, _raw: &RawDoc) -> bool {
        true
    }
    fn process(&self, raw: &mut RawDoc) -> Result<()>;
}

/// 文档插件：某文件类型的完整处理器（组装 RawDoc → IR Document）。
pub trait FormatPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn formats(&self) -> &[&str];
    fn default_parse_chain(&self) -> Vec<&'static str>;
    fn default_process_chain(&self) -> Vec<&'static str>;
    fn assemble(&self, raw: &RawDoc) -> crate::model::Document;

    /// 组装后置钩子：插件把格式特有元数据（frontmatter / 书籍元数据）写入
    /// `doc.meta.extra`。默认空实现（既有插件零改动）；engine 在 `assemble`
    /// 与 `finalize` 之间调用。
    ///
    /// 设计依据：`assemble(&self, raw: &RawDoc) -> Document` 按值返回、返回前
    /// 无处回填元数据；`parse` 阶段（ParseCapability）只有 RawDoc 这层无格式
    /// 语义的载体，拿不到插件对 title/authors/language 的解释权——因此最小
    /// 侵入面是这个后置钩子：raw（原文未剥）与 doc（已组装）此刻都在手上。
    fn enrich_meta(&self, _raw: &RawDoc, _doc: &mut crate::model::Document) {}
}
