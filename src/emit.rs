// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① Image 块渲染 [image] 占位；② feature `json` 下 to_json/
// to_json_pretty；③ 新增 to_translation_units（翻译友好输出，docparse-plan §9）。
//! 输出目标：text / markdown / JSON（feature `json`）/ 翻译单元
//!（`to_translation_units`，翻译管线的工作单元与回写锚点）。

use crate::model::{BlockKind, BlockSource, Document};

/// 纯文本：逐页 text，页间空行分隔。
pub fn to_text(doc: &Document) -> String {
    doc.pages
        .iter()
        .filter(|p| !p.text.trim().is_empty())
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// JSON（紧凑）。feature `json`；等价 `serde_json::to_string`，提供统一出口。
#[cfg(feature = "json")]
pub fn to_json(doc: &Document) -> String {
    serde_json::to_string(doc).expect("Document 序列化不失败")
}

/// JSON（缩进美化）。
#[cfg(feature = "json")]
pub fn to_json_pretty(doc: &Document) -> String {
    serde_json::to_string_pretty(doc).expect("Document 序列化不失败")
}

fn table_md(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let norm: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let mut v = r.clone();
            while v.len() < width {
                v.push(String::new());
            }
            v
        })
        .collect();
    let cell = |r: &[String]| format!("| {} |", r.join(" | "));
    let mut out = vec![cell(&norm[0])];
    out.push(format!("| {} |", vec!["---"; width].join(" | ")));
    for r in &norm[1..] {
        out.push(cell(r));
    }
    out.join("\n")
}

fn block_md(b: &crate::model::Block) -> String {
    match b.kind {
        BlockKind::Heading => {
            let lvl = (b.level.unwrap_or(1) as usize).clamp(1, 6);
            format!("{} {}", "#".repeat(lvl), b.content)
        }
        BlockKind::List => {
            let prefix = if b.ordered.unwrap_or(false) {
                "1."
            } else {
                "-"
            };
            b.content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| format!("{prefix} {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        BlockKind::Code => format!(
            "```{}\n{}\n```",
            b.lang.clone().unwrap_or_default(),
            b.content
        ),
        BlockKind::Table => b
            .table
            .as_ref()
            .map(|t| table_md(&t.rows))
            .unwrap_or_default(),
        BlockKind::Quote => b
            .content
            .lines()
            .map(|l| format!("> {l}"))
            .collect::<Vec<_>>()
            .join("\n"),
        BlockKind::Image => {
            // 图片块：占位 + 已有 OCR 文本则附注
            match b.content.is_empty() {
                true => "[image]".to_string(),
                false => format!("[image] {}", b.content),
            }
        }
        _ => b.content.clone(),
    }
}

/// Markdown：按块渲染（表格 → Markdown 表格）。
///
/// 相邻同类型列表块（如 HTML 的 `<li>` 逐条产出）合并为一个列表，避免中间插空行
/// 被渲染成多个「松散列表」；跨页不合并。
pub fn to_markdown(doc: &Document) -> String {
    let mut out: Vec<String> = Vec::new();
    for page in &doc.pages {
        let mut prev_list: Option<bool> = None;
        for b in page
            .blocks
            .iter()
            // Image 块 content 可为空（未 OCR）但仍渲染占位符
            .filter(|b| !b.content.is_empty() || b.table.is_some() || b.kind == BlockKind::Image)
        {
            let rendered = block_md(b);
            if rendered.is_empty() {
                continue;
            }
            if b.kind == BlockKind::List {
                let ordered = b.ordered.unwrap_or(false);
                if prev_list == Some(ordered) {
                    if let Some(last) = out.last_mut() {
                        last.push('\n');
                        last.push_str(&rendered);
                    }
                    continue;
                }
                prev_list = Some(ordered);
            } else {
                prev_list = None;
            }
            out.push(rendered);
        }
    }
    out.join("\n\n")
}

// ---------------------------------------------------------------------------
// 翻译友好输出（docparse-plan §9）：段落级工作单元，带稳定 ID 与章节上下文
// ---------------------------------------------------------------------------

/// 翻译单元：翻译管线的工作单元。`id` 是回写锚点（译后按 id 对齐双语）。
#[derive(Debug, Clone, PartialEq)]
pub struct TranslationUnit {
    /// 稳定 ID：`p{页号}-b{块序}`（页号取 Page.name，无名为页序）
    pub id: String,
    pub page: Option<String>,
    /// 单元种类（翻译通常只处理 Heading/Paragraph/Quote/List）
    pub kind: BlockKind,
    pub text: String,
    /// 文本来源：TextLayer（零失真）/ Ocr（识别，看 confidence）/ Native
    pub source: BlockSource,
    pub confidence: Option<f32>,
    pub lang: Option<String>,
    /// 最近的上游 Heading 文本（章节上下文，供术语一致性；无则 None）
    pub chapter: Option<String>,
}

impl TranslationUnit {
    /// 是否为 OCR 识别文本（翻译前可按置信度决定人工复核）。
    pub fn is_ocr(&self) -> bool {
        self.source == BlockSource::Ocr
    }
}

/// 可翻译块白名单：Heading / Paragraph / Quote / List（Code/Table/Note 等
/// 结构性内容默认不进翻译流）。
pub fn is_translatable(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Heading | BlockKind::Paragraph | BlockKind::Quote | BlockKind::List)
}

/// 提取翻译单元：按文档序遍历块，白名单 + 最小字符数过滤。
///
/// `min_chars` 过滤超短块（默认 2：滤掉单字符噪声，保留绝大多数实词句）。
/// 章节上下文 = 该块之前最近的 Heading 块文本（跨页不追踪）。
pub fn to_translation_units(doc: &Document) -> Vec<TranslationUnit> {
    to_translation_units_with(doc, 2)
}

/// [`to_translation_units`] 带自定义最小字符数。
pub fn to_translation_units_with(doc: &Document, min_chars: usize) -> Vec<TranslationUnit> {
    let mut out = Vec::new();
    for (p_idx, page) in doc.pages.iter().enumerate() {
        let page_name = page
            .name
            .clone()
            .unwrap_or_else(|| p_idx.to_string());
        let mut chapter: Option<String> = None;
        for (b_idx, b) in page.blocks.iter().enumerate() {
            if b.kind == BlockKind::Heading {
                chapter = Some(b.content.clone());
            }
            if !is_translatable(b.kind) || b.content.trim().is_empty() {
                continue;
            }
            if b.content.chars().count() < min_chars {
                continue;
            }
            out.push(TranslationUnit {
                id: format!("p{page_name}-b{b_idx}"),
                page: Some(page_name.clone()),
                kind: b.kind,
                text: b.content.clone(),
                source: b.source,
                confidence: b.confidence,
                lang: b.lang.clone(),
                chapter: chapter.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, BlockKind, Document, Page};

    fn list(ordered: bool, content: &str) -> Block {
        Block {
            kind: BlockKind::List,
            content: content.to_string(),
            ordered: Some(ordered),
            ..Default::default()
        }
    }

    #[test]
    fn adjacent_lists_merge_same_kind_only() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    Block {
                        kind: BlockKind::Heading,
                        content: "T".into(),
                        level: Some(2),
                        ..Default::default()
                    },
                    list(false, "a"),
                    list(false, "b"),
                    list(true, "x"),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let md = to_markdown(&doc);
        assert_eq!(md, "## T\n\n- a\n- b\n\n1. x");
    }

    #[test]
    fn table_rendered_as_markdown() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![Block {
                    kind: BlockKind::Table,
                    table: Some(crate::model::TableData {
                        rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(to_markdown(&doc), "| a | b |\n| --- | --- |\n| 1 | 2 |");
    }

    #[test]
    fn image_block_renders_placeholder() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    Block { kind: BlockKind::Image, ..Default::default() },
                    Block {
                        kind: BlockKind::Image,
                        content: "图内文字".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let md = to_markdown(&doc);
        assert!(md.contains("[image]\n\n[image] 图内文字"), "{md}");
    }

    #[test]
    fn translation_units_carry_chapter_context() {
        use crate::model::{BlockKind::*, BlockSource};
        let heading = |t: &str| Block {
            kind: Heading,
            content: t.into(),
            level: Some(1),
            ..Default::default()
        };
        let para = |t: &str| Block {
            kind: Paragraph,
            content: t.into(),
            ..Default::default()
        };
        let doc = Document {
            pages: vec![Page {
                name: Some("3".into()),
                blocks: vec![
                    heading("第二章 夜行"),
                    para("他提着灯走过长街。"),
                    para("巷口的猫叫了一声。"),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 3, "标题与正文都进翻译流");
        assert_eq!(units[0].id, "p3-b0");
        assert_eq!(units[0].chapter.as_deref(), Some("第二章 夜行"));
        assert_eq!(units[1].chapter.as_deref(), Some("第二章 夜行"));
        assert_eq!(units[1].page.as_deref(), Some("3"));
        assert_eq!(units[1].source, BlockSource::Native);
        assert!(!units[1].is_ocr());
    }

    #[test]
    fn translation_units_skip_code_and_short() {
        use crate::model::BlockKind::*;
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    Block { kind: Code, content: "let x = 1;".into(), ..Default::default() },
                    Block { kind: Paragraph, content: "甲".into(), ..Default::default() }, // 1 字符 < 2
                    Block { kind: Paragraph, content: "正式句子。".into(), ..Default::default() },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].text, "正式句子。");
    }

    #[test]
    #[cfg(feature = "json")]
    fn to_json_roundtrip() {
        let doc = Document::default();
        let s = to_json(&doc);
        let back: Document = serde_json::from_str(&s).unwrap();
        assert_eq!(back, doc);
    }
}
