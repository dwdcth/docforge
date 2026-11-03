// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① Block 新增 confidence/source；② Page 新增 coord_units；
// ③ 新增 BlockSource/CoordUnits 类型；④ 新增 6 个对应单测；
// ⑤ Document 新增 blocks() 跨页扁平迭代（+ 对应单测）；
// ⑥ Block 新增 quad（OCR 旋转保真四点框，rumi 集成增强）+ 对应单测。其余逐字保留。
//! 统一 IR（含 serde 便于 JSON/CLI 往返；块/元信息语义与 docparser 的
//! Python 参考实现一致，docforge 在其上扩展 OCR 语义字段）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 轴对齐矩形，坐标语义由产生方决定（页面 px / pt / 逻辑单位）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    Heading,
    #[default]
    Paragraph,
    List,
    Code,
    Table,
    Image,
    Quote,
    /// 图表（pptx `c:chart` 等）：`content` 为图表内文本（标题 + 数据标签）
    Chart,
    /// 备注 / 批注 / 脚注等附属文本（对齐 Python `BlockKind.NOTE`）：
    /// `meta["region"]` 标注来源区域（如 pptx 备注页 = `"notes"`）。
    Note,
}

/// 块来源：消费者据此区分"零失真文本"（TextLayer / Native）与"识别文本"（Ocr）。
/// rumi 翻译/双层 PDF 等下游按此走不同信任级处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockSource {
    /// PDF 文字层直提（PDF 点坐标、左下原点，契约见 pdf 后端模块头注释）
    TextLayer,
    /// 渲染 + OCR 识别（置信度见 `Block.confidence`）
    Ocr,
    /// 原生结构（DOCX/EPUB 等格式自带，或 TXT 等按行重组）
    #[default]
    Native,
}

/// 页面几何的坐标单位与语义（消费者换算依据；缺省 None = 该页无几何/未标注）。
/// docparser 的"坐标语义由产生方决定"对消费者不友好，docforge 显式标注
/// （归一化坐标列为远期重构项，docparse-plan §14 Q1）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoordUnits {
    /// PDF 点（1/72 inch），左下原点、y 向上（PDF 文字层契约）
    Pt,
    /// 渲染像素，左上原点；`dpi` 为渲染分辨率（OCR 输入图）
    Px { dpi: f32 },
    /// 格式逻辑单位（docx twip/EMU 等）或流式无几何
    Logical,
}

/// 结构化表格：`rows[row][col]` = 单元格文本。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TableData {
    pub rows: Vec<Vec<String>>,
    /// 前 N 行为表头（0 = 未识别）。与 Python `TableData.header_rows` 同口径：
    /// 目前只有 xlsx 冻结窗格（`<pane state="frozen" ySplit>0`）会给出 1。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub header_rows: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// 块级元信息的值（与 Python `Block.meta: Dict[str, Any]` 的**实际取值形态**同形）：
/// 只允许布尔 / 字符串 / 字符串数组三种，序列化后分别是 `true` / `"x"` / `["a"]`。
///
/// 刻意不用 `serde_json::Value`：`docforge` 的运行时依赖只有 `serde`，
/// 为了一个 meta 字段把 JSON 值模型拉进内核，会让「裸内核依赖预算」破功。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MetaValue {
    /// `truncated` 一类开关
    Bool(bool),
    /// `region` 一类单值标注
    Str(String),
    /// `merges` 一类字符串列表（xlsx 合并区域 `A1:B2`）
    List(Vec<String>),
}

impl From<bool> for MetaValue {
    fn from(v: bool) -> Self {
        MetaValue::Bool(v)
    }
}

impl From<String> for MetaValue {
    fn from(v: String) -> Self {
        MetaValue::Str(v)
    }
}

impl From<&str> for MetaValue {
    fn from(v: &str) -> Self {
        MetaValue::Str(v.to_string())
    }
}

impl From<Vec<String>> for MetaValue {
    fn from(v: Vec<String>) -> Self {
        MetaValue::List(v)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Block {
    #[serde(rename = "kind")]
    pub kind: BlockKind,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Rect>,
    /// 旋转保真四点框（顺时针自左上）。轴对齐文本为 None；OCR 旋转/倾斜行有值。
    /// bbox 恒为轴对齐包围盒，quad 是它的超集补充（PaddleOCR det polys 天然形态）。
    /// 坐标语义随 `Page.coord_units`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quad: Option<[[f32; 2]; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordered: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<TableData>,
    /// 块级附加信息（对齐 Python `Block.meta`）：目前 xlsx 表格用
    /// `merges`（合并区域列表）+ `truncated`（是否被预算截断）。
    /// 空表不序列化，避免给所有格式的 JSON 加噪音。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, MetaValue>,
    /// OCR 行置信度 0..1（`source = Ocr` 时有值；TextLayer/Native 为 None）。
    /// 低置信度行的处置（标记 vs 丢弃）由 Options 决定，IR 只忠实携带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// 块来源。serde 缺省 Native：docparser 旧 JSON（无此字段）可无损读入。
    #[serde(default)]
    pub source: BlockSource,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Image {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<Rect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr_text: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Page {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub blocks: Vec<Block>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub images: Vec<Image>,
    /// 页面几何（有物理页概念的格式填写）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<i32>,
    /// 本页 blocks/images 的 bbox 坐标单位（PDF 文字层 Pt、OCR 页 Px{dpi}）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coord_units: Option<CoordUnits>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocMeta {
    pub file_type: String,
    pub page_count: usize,
    pub text_length: usize,
    pub image_count: usize,
    pub image_text_length: usize,
    /// 插件级附加信息：书籍元数据（书名/作者/语言/封面路径）、处理链告警等。
    /// 空时不进 JSON。与 Python 参考实现的 `meta.extra` 对齐。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extra: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub meta: DocMeta,
    #[serde(default)]
    pub pages: Vec<Page>,
}

impl Document {
    /// 回填统计（插件组装完 pages 后调用）。
    pub fn finalize(mut self) -> Self {
        self.meta.page_count = self.pages.len();
        self.meta.text_length = self.pages.iter().map(|p| p.text.chars().count()).sum();
        self.meta.image_count = self.pages.iter().map(|p| p.images.len()).sum();
        self.meta.image_text_length = self
            .pages
            .iter()
            .flat_map(|p| &p.images)
            .map(|im| {
                im.ocr_text
                    .as_deref()
                    .map(|s| s.chars().count())
                    .unwrap_or(0)
            })
            .sum();
        self
    }

    /// 跨页扁平迭代所有块（页序×页内序）。最常见的遍历形态一行化：
    /// `for b in doc.blocks()` 取代手写双层 `pages` × `blocks` 循环。
    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.pages.iter().flat_map(|p| p.blocks.iter())
    }
}

/// 渲染块序列为页文本（表格展开全部行，逗号分列——与 Python `blocks_to_text` 一致）。
pub fn blocks_to_text(blocks: &[Block], sep: &str) -> String {
    let parts: Vec<String> = blocks
        .iter()
        .filter_map(|b| match b.kind {
            BlockKind::Table => b.table.as_ref().map(|t| {
                t.rows
                    .iter()
                    .map(|r| r.join(","))
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
            _ if !b.content.is_empty() => Some(b.content.clone()),
            _ => None,
        })
        .collect();
    parts.join(sep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_finalize_counts() {
        let doc = Document {
            pages: vec![
                Page {
                    text: "你好".into(),
                    images: vec![Image {
                        ocr_text: Some("x".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                Page {
                    text: "world".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
        .finalize();
        assert_eq!(doc.meta.page_count, 2);
        assert_eq!(doc.meta.text_length, 7); // "你好world"
        assert_eq!(doc.meta.image_count, 1);
        assert_eq!(doc.meta.image_text_length, 1);
    }

    /// `Document::blocks()`：跨页扁平迭代按 页序×页内序 排列，不乱序；
    /// 只借用 `&self`，迭代后文档本体仍可用。
    #[test]
    fn blocks_flattens_pages_in_order() {
        let block = |c: &str| Block {
            kind: BlockKind::Paragraph,
            content: c.into(),
            ..Default::default()
        };
        let doc = Document {
            pages: vec![
                Page {
                    blocks: vec![block("页1b0"), block("页1b1")],
                    ..Default::default()
                },
                Page {
                    blocks: vec![block("页2b0"), block("页2b1")],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let contents: Vec<&str> = doc.blocks().map(|b| b.content.as_str()).collect();
        assert_eq!(contents, ["页1b0", "页1b1", "页2b0", "页2b1"]);
        assert_eq!(doc.pages.len(), 2);
    }

    #[test]
    fn blocks_to_text_expands_tables() {
        let b = Block {
            kind: BlockKind::Table,
            table: Some(TableData {
                rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
                ..Default::default()
            }),
            ..Default::default()
        };
        let txt = blocks_to_text(&[b], "\n\n");
        assert_eq!(txt, "a,b\n1,2");
    }

    /// `Block.meta` 的 JSON 形态必须与 Python `Block.meta` 一致：
    /// bool / 字符串 / 字符串数组都是裸值，不加 `{type, value}` 包装。
    #[test]
    fn block_meta_serializes_like_python() {
        let mut meta = BTreeMap::new();
        meta.insert("merges".to_string(), vec!["A1:B1".to_string()].into());
        meta.insert("truncated".to_string(), false.into());
        meta.insert("region".to_string(), "notes".into());
        let block = Block {
            kind: BlockKind::Table,
            meta,
            ..Default::default()
        };

        let json = serde_json::to_string(&block).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["meta"]["merges"], serde_json::json!(["A1:B1"]));
        assert_eq!(v["meta"]["truncated"], serde_json::json!(false));
        assert_eq!(v["meta"]["region"], serde_json::json!("notes"));

        let back: Block = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    /// 空 meta 不序列化：不能给所有格式的 JSON 平白加一个 `"meta": {}`。
    #[test]
    fn empty_meta_is_omitted() {
        let json = serde_json::to_string(&Block::default()).unwrap();
        assert!(!json.contains("meta"), "{json}");
    }

    /// `BlockKind` 的线上取值必须与 Python `BlockKind` 枚举逐字一致
    /// （Python 侧有同名单测遍历枚举值域，两边必须同时绿）。
    #[test]
    fn block_kind_wire_values_match_python() {
        let cases = [
            (BlockKind::Heading, "heading"),
            (BlockKind::Paragraph, "paragraph"),
            (BlockKind::List, "list"),
            (BlockKind::Code, "code"),
            (BlockKind::Table, "table"),
            (BlockKind::Image, "image"),
            (BlockKind::Quote, "quote"),
            (BlockKind::Chart, "chart"),
            (BlockKind::Note, "note"),
        ];
        for (kind, wire) in cases {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{wire}\""), "{kind:?}");
            assert_eq!(serde_json::from_str::<BlockKind>(&json).unwrap(), kind);
        }
    }

    #[test]
    fn roundtrip_json() {
        let doc = Document {
            pages: vec![Page {
                text: "t".into(),
                blocks: vec![Block {
                    kind: BlockKind::Paragraph,
                    content: "hi".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&s).unwrap();
        assert_eq!(back, doc);
    }

    /// `BlockSource` 的线上取值 snake_case：text_layer / ocr / native。
    #[test]
    fn block_source_wire_values() {
        let cases = [
            (BlockSource::TextLayer, "text_layer"),
            (BlockSource::Ocr, "ocr"),
            (BlockSource::Native, "native"),
        ];
        for (source, wire) in cases {
            let json = serde_json::to_string(&source).unwrap();
            assert_eq!(json, format!("\"{wire}\""), "{source:?}");
            assert_eq!(serde_json::from_str::<BlockSource>(&json).unwrap(), source);
        }
    }

    /// docparser 旧 JSON（无 source 键）读入默认 Native：wire 兼容承诺。
    #[test]
    fn block_source_defaults_to_native() {
        let block: Block = serde_json::from_str(r#"{"kind":"paragraph"}"#).unwrap();
        assert_eq!(block.source, BlockSource::Native);
    }

    /// confidence 仅在有值时序列化。
    #[test]
    fn block_confidence_serializes_only_when_present() {
        let plain = serde_json::to_string(&Block::default()).unwrap();
        assert!(!plain.contains("confidence"), "{plain}");

        let block = Block {
            confidence: Some(0.87),
            ..Default::default()
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"confidence\":0.87"), "{json}");
        let back: Block = serde_json::from_str(&json).unwrap();
        assert_eq!(back.confidence, Some(0.87));
    }

    /// `Block.quad`：None 不出键（docparser 旧 JSON wire 兼容）；Some 完整
    /// 往返（顺时针自左上的四点框按数组形态逐点还原）。
    #[test]
    fn block_quad_wire_omitted_and_roundtrip() {
        let plain = serde_json::to_string(&Block::default()).unwrap();
        assert!(!plain.contains("quad"), "{plain}");
        // 旧 JSON（无 quad 键）读入默认 None
        let old: Block = serde_json::from_str(r#"{"kind":"paragraph"}"#).unwrap();
        assert_eq!(old.quad, None);

        let quad = [[10.0, 5.0], [20.0, 6.0], [19.0, 15.0], [9.0, 14.0]];
        let block = Block {
            quad: Some(quad),
            ..Default::default()
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"quad\":[[10.0,5.0]"), "{json}");
        let back: Block = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
        assert_eq!(back.quad.as_ref().unwrap()[2], [19.0, 15.0]);
    }

    /// `CoordUnits` 的 wire 形态：内部标签 type + snake_case 变体名。
    #[test]
    fn page_coord_units_wire() {
        let cases = [
            (CoordUnits::Pt, r#"{"type":"pt"}"#),
            (CoordUnits::Px { dpi: 144.0 }, r#"{"type":"px","dpi":144.0}"#),
            (CoordUnits::Logical, r#"{"type":"logical"}"#),
        ];
        for (units, wire) in cases {
            let json = serde_json::to_string(&units).unwrap();
            assert_eq!(json, wire, "{units:?}");
            assert_eq!(serde_json::from_str::<CoordUnits>(&json).unwrap(), units);
        }
    }

    /// coord_units 为 None 时不出键。
    #[test]
    fn page_coord_units_omitted_when_none() {
        let json = serde_json::to_string(&Page::default()).unwrap();
        assert!(!json.contains("coord_units"), "{json}");
    }

    /// OCR 语义三字段（confidence/source/coord_units）全量 serde 往返。
    #[test]
    fn roundtrip_json_with_ocr_fields() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    Block {
                        kind: BlockKind::Paragraph,
                        content: "识别行".into(),
                        confidence: Some(0.92),
                        source: BlockSource::Ocr,
                        ..Default::default()
                    },
                    Block {
                        kind: BlockKind::Paragraph,
                        content: "文字层行".into(),
                        source: BlockSource::TextLayer,
                        ..Default::default()
                    },
                ],
                coord_units: Some(CoordUnits::Px { dpi: 144.0 }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&s).unwrap();
        assert_eq!(back, doc);
        // 两个来源块都在，置信度不串位
        assert_eq!(back.pages[0].blocks[0].confidence, Some(0.92));
        assert_eq!(back.pages[0].blocks[1].source, BlockSource::TextLayer);
        assert_eq!(
            back.pages[0].coord_units,
            Some(CoordUnits::Px { dpi: 144.0 })
        );
    }
}
