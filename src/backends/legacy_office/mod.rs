// 老式二进制 Office 后端（.doc / .ppt / .xls / .xlsb），
// 核心解析模块吸收自 firecrawl/anydoc 0.2.5（MIT），见 THIRD_PARTY_NOTICES.md。
//! 老式二进制 Office 后端（四个细粒度 feature：`doc` / `ppt` / `xls` / `xlsb` +
//! 聚合 feature `legacy-office`）：
//! - `doc`：Word 97-2003 二进制（OLE2 FIB / piece table / STSH / CHPX / PAPX / 列表 / 表格）
//! - `ppt`：PowerPoint 97-2003 二进制（OLE2 CurrentUser / Persist / Slide / Master / Notes）
//! - `xls`：Excel 97-2003 BIFF8（OLE2 Workbook / SST / XF / NumFmt / 合并格 / 复选框）
//! - `xlsb`：Excel 二进制工作簿（ZIP OPC + MS-XLSB 记录流 / NumFmt / 合并格）

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use crate::error::{DocError, Result};
use crate::model::{
    blocks_to_text, Block, BlockKind, BlockSource, Document, Image, MetaValue, Page, TableData,
};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawImage, RawPage, RawRegion, RawTextItem};
use crate::registry::Registry;

mod error;
mod formats;
mod model;
mod package;
mod shared;

/// 本编译配置实际启用的老式 Office 格式列表。
pub const FORMATS: &[&str] = &[
    #[cfg(feature = "doc")]
    "doc",
    #[cfg(feature = "ppt")]
    "ppt",
    #[cfg(feature = "xls")]
    "xls",
    #[cfg(feature = "xlsb")]
    "xlsb",
];

impl From<error::ConvertError> for DocError {
    fn from(e: error::ConvertError) -> Self {
        match e {
            error::ConvertError::Encrypted => DocError::encrypted("加密的老式 Office 文档"),
            error::ConvertError::ResourceLimit { limit, detail } => {
                DocError::resource_limit(format!("{limit}: {detail}"))
            }
            error::ConvertError::Unsupported(what) => DocError::unsupported(what),
            other => DocError::corrupt(other.to_string()),
        }
    }
}

/// 老式 Office 统一解析能力（按编译期 feature 支持 `doc` / `ppt` / `xls` / `xlsb`）。
#[derive(Default)]
pub struct LegacyOfficeBackend {
    /// 是否保留内嵌图片字节进 `RawPage.images`（供 OCR 处理链与 OKF 资产落盘使用）。
    pub ocr_images: bool,
}

impl LegacyOfficeBackend {
    pub fn with_ocr_images(mut self, yes: bool) -> Self {
        self.ocr_images = yes;
        self
    }

    fn parse_by_format(&self, fmt: &str, bytes: &[u8]) -> Result<RawDoc> {
        match fmt {
            #[cfg(feature = "doc")]
            "doc" => {
                let doc = formats::doc::parse(bytes)?;
                Ok(convert_document(doc, LayoutKind::Flow, self.ocr_images))
            }
            #[cfg(feature = "ppt")]
            "ppt" => {
                let doc = formats::ppt::parse(bytes)?;
                Ok(convert_document(doc, LayoutKind::Slides, self.ocr_images))
            }
            #[cfg(feature = "xls")]
            "xls" => {
                let doc = formats::sheet::parse_xls(bytes)?;
                Ok(convert_document(doc, LayoutKind::Workbook, self.ocr_images))
            }
            #[cfg(feature = "xlsb")]
            "xlsb" => {
                let doc = formats::sheet::parse_xlsb(bytes)?;
                Ok(convert_document(doc, LayoutKind::Workbook, self.ocr_images))
            }
            other => Err(DocError::unsupported(format!(
                "当前构建未启用老式 Office 格式: {other}"
            ))),
        }
    }
}

impl ParseCapability for LegacyOfficeBackend {
    fn id(&self) -> &str {
        "legacy-office"
    }

    fn formats(&self) -> &[&str] {
        FORMATS
    }

    fn parse(&self, path: &Path) -> Result<RawDoc> {
        let fmt = crate::router::sniff(path)?;
        let bytes = std::fs::read(path)
            .map_err(|e| DocError::corrupt(format!("cannot read {}: {e}", path.display())))?;
        self.parse_by_format(fmt, &bytes)
    }
}

/// 老式 Office 插件：把 `RawDoc` 组装为统一 IR `Document`。
pub struct LegacyOfficePlugin;

impl FormatPlugin for LegacyOfficePlugin {
    fn id(&self) -> &str {
        "legacy-office"
    }

    fn formats(&self) -> &[&str] {
        FORMATS
    }

    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["legacy-office"]
    }

    fn default_process_chain(&self) -> Vec<&'static str> {
        #[allow(unused_mut)]
        let mut chain: Vec<&'static str> = vec!["ocr-images"];
        #[cfg(feature = "lang-detect")]
        chain.push("lang-detect");
        chain
    }

    fn assemble(&self, raw: &RawDoc) -> Document {
        let pages: Vec<Page> = raw
            .pages
            .iter()
            .map(|rp| {
                let mut blocks = Vec::new();
                for it in &rp.items {
                    if let Some(ref im) = it.image {
                        blocks.push(Block {
                            kind: BlockKind::Image,
                            content: im.ocr_text.clone().unwrap_or_default(),
                            meta: it.meta.clone(),
                            source: BlockSource::Native,
                            ..Default::default()
                        });
                        continue;
                    }
                    if let Some(ref table) = it.table {
                        blocks.push(Block {
                            kind: BlockKind::Table,
                            content: it.text.clone(),
                            table: Some(table.clone()),
                            meta: it.meta.clone(),
                            source: BlockSource::Native,
                            ..Default::default()
                        });
                    } else if let Some(lvl) = it.level {
                        blocks.push(Block {
                            kind: BlockKind::Heading,
                            level: Some(lvl),
                            content: it.text.clone(),
                            meta: it.meta.clone(),
                            source: BlockSource::Native,
                            ..Default::default()
                        });
                    } else if let Some(ordered) = it.list {
                        blocks.push(Block {
                            kind: BlockKind::List,
                            ordered: Some(ordered),
                            content: it.text.clone(),
                            meta: it.meta.clone(),
                            source: BlockSource::Native,
                            ..Default::default()
                        });
                    } else if it.meta.get("block_kind")
                        == Some(&MetaValue::Str("code".to_string()))
                    {
                        let lang = it.meta.get("lang").and_then(|v| match v {
                            MetaValue::Str(s) => Some(s.clone()),
                            _ => None,
                        });
                        blocks.push(Block {
                            kind: BlockKind::Code,
                            lang,
                            content: it.text.clone(),
                            source: BlockSource::Native,
                            ..Default::default()
                        });
                    } else if it.meta.get("block_kind")
                        == Some(&MetaValue::Str("quote".to_string()))
                    {
                        blocks.push(Block {
                            kind: BlockKind::Quote,
                            content: it.text.clone(),
                            source: BlockSource::Native,
                            ..Default::default()
                        });
                    } else if !it.text.is_empty() {
                        blocks.push(Block {
                            kind: BlockKind::Paragraph,
                            content: it.text.clone(),
                            meta: it.meta.clone(),
                            source: BlockSource::Native,
                            ..Default::default()
                        });
                    }
                }
                for reg in &rp.regions {
                    if !reg.text.trim().is_empty() {
                        let mut meta = std::collections::BTreeMap::new();
                        meta.insert("region".to_string(), MetaValue::Str(reg.kind.clone()));
                        blocks.push(Block {
                            kind: BlockKind::Note,
                            content: reg.text.clone(),
                            meta,
                            source: BlockSource::Native,
                            ..Default::default()
                        });
                    }
                }
                let text = blocks_to_text(&blocks, "\n\n");
                let images: Vec<Image> = rp
                    .images
                    .iter()
                    .map(|im| Image {
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
                    ..Default::default()
                }
            })
            .collect();
        Document {
            pages,
            ..Default::default()
        }
    }
}

/// 注册老式 Office 后端与插件（默认不提取内嵌图片字节）。
pub fn register_defaults(reg: &mut Registry) {
    register_with_ocr_images(reg, false);
}

/// 注册老式 Office 后端与插件（可选开启内嵌图片字节提取）。
pub fn register_with_ocr_images(reg: &mut Registry, ocr_images: bool) {
    if FORMATS.is_empty() {
        return;
    }
    let _ = reg.register_parse(
        Arc::new(LegacyOfficeBackend::default().with_ocr_images(ocr_images)),
        None,
    );
    reg.register_plugin(Arc::new(LegacyOfficePlugin));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutKind {
    Flow,
    Slides,
    Workbook,
}

fn convert_document(doc: model::Document, layout: LayoutKind, ocr_images: bool) -> RawDoc {
    let assets = &doc.assets;
    let mut pages: Vec<RawPage> = Vec::new();

    match layout {
        LayoutKind::Workbook => {
            let mut cur_name: Option<String> = None;
            let mut cur_items: Vec<RawTextItem> = Vec::new();
            for blk in &doc.blocks {
                match blk {
                    model::Block::Heading { content, .. } => {
                        if !cur_items.is_empty() {
                            pages.push(RawPage {
                                name: cur_name.take(),
                                items: std::mem::take(&mut cur_items),
                                ..Default::default()
                            });
                        }
                        let name = model::inlines_to_plain_text(content).trim().to_string();
                        cur_name = if name.is_empty() { None } else { Some(name) };
                    }
                    other => push_block(other, assets, ocr_images, &mut cur_items, &mut Vec::new()),
                }
            }
            if !cur_items.is_empty() || cur_name.is_some() {
                pages.push(RawPage {
                    name: cur_name,
                    items: cur_items,
                    ..Default::default()
                });
            }
        }
        LayoutKind::Slides => {
            let mut slide_idx = 1usize;
            let mut cur_items: Vec<RawTextItem> = Vec::new();
            let mut cur_images: Vec<RawImage> = Vec::new();
            for blk in &doc.blocks {
                if matches!(blk, model::Block::Rule) {
                    if !cur_items.is_empty() || !cur_images.is_empty() {
                        pages.push(RawPage {
                            name: Some(format!("slide-{slide_idx}")),
                            items: std::mem::take(&mut cur_items),
                            images: std::mem::take(&mut cur_images),
                            ..Default::default()
                        });
                        slide_idx += 1;
                    }
                } else {
                    push_block(blk, assets, ocr_images, &mut cur_items, &mut cur_images);
                }
            }
            if !cur_items.is_empty() || !cur_images.is_empty() {
                pages.push(RawPage {
                    name: Some(format!("slide-{slide_idx}")),
                    items: cur_items,
                    images: cur_images,
                    ..Default::default()
                });
            }
        }
        LayoutKind::Flow => {
            let mut items: Vec<RawTextItem> = Vec::new();
            let mut images: Vec<RawImage> = Vec::new();
            for blk in &doc.blocks {
                push_block(blk, assets, ocr_images, &mut items, &mut images);
            }
            let regions: Vec<RawRegion> = doc
                .notes
                .iter()
                .filter_map(|n| {
                    let text = flatten_blocks_text(&n.blocks);
                    if text.trim().is_empty() {
                        return None;
                    }
                    let kind = match n.kind {
                        model::NoteKind::Footnote => "footnote",
                        model::NoteKind::Endnote => "endnote",
                    };
                    Some(RawRegion {
                        kind: kind.to_string(),
                        text,
                    })
                })
                .collect();
            pages.push(RawPage {
                name: None,
                items,
                images,
                regions,
                ..Default::default()
            });
        }
    }

    if pages.is_empty() {
        pages.push(RawPage::default());
    }

    RawDoc {
        pages,
        ..Default::default()
    }
}

fn push_block(
    blk: &model::Block,
    assets: &[model::Asset],
    ocr_images: bool,
    items: &mut Vec<RawTextItem>,
    images: &mut Vec<RawImage>,
) {
    match blk {
        model::Block::Heading { level, content, .. } => {
            let text = render_inlines_text(content);
            if !text.trim().is_empty() {
                items.push(RawTextItem {
                    text: text.trim().to_string(),
                    level: Some((*level).clamp(1, 6)),
                    ..Default::default()
                });
            }
        }
        model::Block::Paragraph(inlines) => {
            let text = render_inlines_text(inlines);
            if !text.trim().is_empty() {
                items.push(RawTextItem {
                    text: text.trim().to_string(),
                    ..Default::default()
                });
            }
            if ocr_images {
                collect_inline_images(inlines, assets, items, images);
            }
        }
        model::Block::List(list) => {
            let ordered = !matches!(list.marker, model::MarkerKind::Bullet);
            let mut lines = Vec::new();
            for item in &list.items {
                let t = flatten_blocks_text(&item.blocks);
                if !t.trim().is_empty() {
                    lines.push(t.trim().to_string());
                }
            }
            if !lines.is_empty() {
                items.push(RawTextItem {
                    text: lines.join("\n"),
                    list: Some(ordered),
                    ..Default::default()
                });
            }
        }
        model::Block::Table(table) => {
            let rows = flatten_table_rows(table);
            if !rows.is_empty() {
                let text = rows
                    .iter()
                    .map(|r| r.join(","))
                    .collect::<Vec<_>>()
                    .join("\n");
                items.push(RawTextItem {
                    text,
                    table: Some(TableData {
                        rows,
                        header_rows: table.header_rows,
                    }),
                    ..Default::default()
                });
            }
        }
        model::Block::BlockQuote(inner) => {
            let text = flatten_blocks_text(inner);
            if !text.trim().is_empty() {
                let mut meta = std::collections::BTreeMap::new();
                meta.insert("block_kind".to_string(), MetaValue::Str("quote".to_string()));
                items.push(RawTextItem {
                    text: text.trim().to_string(),
                    meta,
                    ..Default::default()
                });
            }
        }
        model::Block::CodeBlock { lang, text } => {
            if !text.is_empty() {
                let mut meta = std::collections::BTreeMap::new();
                meta.insert("block_kind".to_string(), MetaValue::Str("code".to_string()));
                if let Some(l) = lang {
                    meta.insert("lang".to_string(), MetaValue::Str(l.clone()));
                }
                items.push(RawTextItem {
                    text: text.clone(),
                    meta,
                    ..Default::default()
                });
            }
        }
        model::Block::Math(tex) => {
            let t = tex.trim();
            if !t.is_empty() {
                items.push(RawTextItem {
                    text: format!("$${t}$$"),
                    ..Default::default()
                });
            }
        }
        model::Block::Rule => {}
    }
}

fn collect_inline_images(
    inlines: &[model::Inline],
    assets: &[model::Asset],
    items: &mut Vec<RawTextItem>,
    images: &mut Vec<RawImage>,
) {
    for inline in inlines {
        match inline {
            model::Inline::Image {
                alt,
                source: model::ImageSource::Asset(id),
            } => {
                if let Some(asset) = assets.get(id.0) {
                    let ext = asset
                        .media_type
                        .rsplit('/')
                        .next()
                        .unwrap_or("bin")
                        .to_string();
                    let raw_img = RawImage {
                        data: asset.bytes.clone(),
                        ext,
                        ..Default::default()
                    };
                    images.push(raw_img.clone());
                    let mut meta = std::collections::BTreeMap::new();
                    if !alt.trim().is_empty() {
                        meta.insert("alt".to_string(), MetaValue::Str(alt.trim().to_string()));
                    }
                    items.push(RawTextItem {
                        image: Some(raw_img),
                        meta,
                        ..Default::default()
                    });
                }
            }
            model::Inline::Link { content, .. } => {
                collect_inline_images(content, assets, items, images);
            }
            _ => {}
        }
    }
}

fn render_inlines_text(inlines: &[model::Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            model::Inline::Text { text, .. } => out.push_str(text),
            model::Inline::Link { content, .. } => out.push_str(&render_inlines_text(content)),
            model::Inline::Image { alt, .. } => out.push_str(alt),
            model::Inline::Math(tex) => {
                let t = tex.trim();
                if !t.is_empty() {
                    out.push('$');
                    out.push_str(t);
                    out.push('$');
                }
            }
            model::Inline::Checkbox(checked) => out.push_str(model::checkbox_text(*checked)),
            model::Inline::LineBreak => out.push('\n'),
            model::Inline::Anchor(_) | model::Inline::NoteRef(_) => {}
        }
    }
    out
}

fn flatten_blocks_text(blocks: &[model::Block]) -> String {
    let mut parts = Vec::new();
    for b in blocks {
        match b {
            model::Block::Heading { content, .. } | model::Block::Paragraph(content) => {
                let t = render_inlines_text(content);
                if !t.trim().is_empty() {
                    parts.push(t.trim().to_string());
                }
            }
            model::Block::List(list) => {
                for item in &list.items {
                    let t = flatten_blocks_text(&item.blocks);
                    if !t.trim().is_empty() {
                        parts.push(t);
                    }
                }
            }
            model::Block::BlockQuote(inner) => {
                let t = flatten_blocks_text(inner);
                if !t.trim().is_empty() {
                    parts.push(t);
                }
            }
            model::Block::CodeBlock { text, .. } => {
                if !text.trim().is_empty() {
                    parts.push(text.clone());
                }
            }
            model::Block::Math(tex) => {
                if !tex.trim().is_empty() {
                    parts.push(format!("$${}$$", tex.trim()));
                }
            }
            model::Block::Table(table) => {
                let rows = flatten_table_rows(table);
                for r in rows {
                    parts.push(r.join(", "));
                }
            }
            model::Block::Rule => {}
        }
    }
    parts.join("\n")
}

fn flatten_table_rows(table: &model::Table) -> Vec<Vec<String>> {
    table
        .grid
        .iter()
        .map(|row| {
            row.iter()
                .map(|slot| match slot {
                    model::CellSlot::Origin(cell) => flatten_blocks_text(&cell.blocks),
                    model::CellSlot::Covered { .. } => String::new(),
                })
                .collect()
        })
        .collect()
}

/// 当启用 OLE 格式（doc/ppt/xls）时供 `router::sniff` 探测 OLE2 内部流名，
/// 精准区分 WordDocument / PowerPoint Document / Workbook / EncryptedPackage。
#[cfg(any(feature = "doc", feature = "ppt", feature = "xls"))]
pub(crate) fn probe_ole_format(path: &Path) -> Option<std::result::Result<&'static str, DocError>> {
    let file = std::fs::File::open(path).ok()?;
    let ole = cfb::CompoundFile::open(file).ok()?;
    for entry in ole.read_root_storage() {
        let name = entry.name();
        if name.eq_ignore_ascii_case("EncryptedPackage") {
            return Some(Err(DocError::encrypted("加密的 Office 容器（OLE EncryptedPackage）")));
        }
        if name.eq_ignore_ascii_case("WordDocument") {
            return Some(Ok("doc"));
        }
        if name.eq_ignore_ascii_case("PowerPoint Document") {
            return Some(Ok("ppt"));
        }
        if name.eq_ignore_ascii_case("Workbook") || name.eq_ignore_ascii_case("Book") {
            return Some(Ok("xls"));
        }
    }
    None
}

#[cfg(test)]
mod adapter_tests {
    use super::*;

    /// 端到端冒烟：验证 `convert_document` + `LegacyOfficePlugin` 组装出的 IR 能无缝接入
    /// `emit::to_gfm` 与 `okf::to_okf_concept`（含图片资产与工作簿分页）。
    #[test]
    fn adapter_maps_workbook_and_flow_with_assets_into_docforge_ir() {
        let mut t = model::Table::from_rows(
            vec![
                vec![
                    model::Cell::from_inlines(vec![model::Inline::plain("Item")]),
                    model::Cell::from_inlines(vec![model::Inline::plain("Qty")]),
                ],
                vec![
                    model::Cell::from_inlines(vec![model::Inline::plain("Apple")]),
                    model::Cell::from_inlines(vec![model::Inline::plain("3")]),
                ],
            ],
            1,
            model::TableKind::Data,
        );
        t.header_rows = 1;
        let wb = model::Document {
            blocks: vec![
                model::Block::heading(2, vec![model::Inline::plain("Sales")]),
                model::Block::Table(t),
            ],
            notes: vec![],
            assets: vec![],
        };
        let raw = convert_document(wb, LayoutKind::Workbook, false);
        let doc = LegacyOfficePlugin.assemble(&raw).finalize();
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].name.as_deref(), Some("Sales"));
        let gfm = crate::emit::to_gfm(&doc);
        assert!(gfm.contains("| Item | Qty |"), "{gfm}");
        assert!(gfm.contains("| Apple | 3 |"), "{gfm}");

        // Flow 文档带公式与内嵌 PNG 资产 → OKF 输出包含资产与 LaTeX
        let png = b"\x89PNG\r\n\x1a\nFAKE".to_vec();
        let flow = model::Document {
            blocks: vec![
                model::Block::heading(1, vec![model::Inline::plain("Legacy Doc")]),
                model::Block::Paragraph(vec![
                    model::Inline::plain("Formula "),
                    model::Inline::Math("x^2".into()),
                ]),
                model::Block::Paragraph(vec![model::Inline::Image {
                    alt: "chart".into(),
                    source: model::ImageSource::Asset(model::AssetId(0)),
                }]),
            ],
            notes: vec![model::Note {
                id: "1".into(),
                kind: model::NoteKind::Footnote,
                blocks: vec![model::Block::Paragraph(vec![model::Inline::plain(
                    "Note body",
                )])],
            }],
            assets: vec![model::Asset {
                id: model::AssetId(0),
                media_type: "image/png".into(),
                origin_part: "word/media/img1.png".into(),
                bytes: png,
            }],
        };
        let raw_flow = convert_document(flow, LayoutKind::Flow, true);
        let doc_flow = LegacyOfficePlugin.assemble(&raw_flow).finalize();
        let okf = crate::okf::to_okf_concept(&doc_flow, &crate::okf::OkfOptions::default());
        assert_eq!(okf.assets.len(), 1);
        assert!(okf.markdown.contains("Formula $x^2$"), "{}", okf.markdown);
        assert!(okf.markdown.contains("![chart](./assets/"), "{}", okf.markdown);
        assert!(okf.markdown.contains("[^1]: [footnote] Note body"), "{}", okf.markdown);
    }

    #[cfg(feature = "doc")]
    #[test]
    fn encrypted_ole_container_is_detected_as_encrypted() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("docforge_legacy_enc_{}.doc", std::process::id()));
        {
            let mut ole = cfb::CompoundFile::create(std::fs::File::create(&p).unwrap()).unwrap();
            ole.create_stream("EncryptedPackage").unwrap();
            ole.create_stream("EncryptionInfo").unwrap();
        }
        let err = crate::parse(&p).unwrap_err();
        let _ = std::fs::remove_file(&p);
        assert_eq!(err.code, crate::error::ErrorCode::Encrypted);
    }
}
