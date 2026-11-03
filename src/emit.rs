// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① Image 块渲染 [image] 占位；② feature `json` 下 to_json/
// to_json_pretty；③ 新增 to_translation_units（翻译友好输出，docparse-plan §9）；
// ④ 新增 to_rag_chunks（RAG 检索分块，docparse-plan §13 M4）。
//! 输出目标：text / markdown / JSON（feature `json`）/ 翻译单元
//!（`to_translation_units`，翻译管线的工作单元与回写锚点）/ RAG 检索块
//!（`to_rag_chunks`，向量化/检索管线的工作单元）。

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

// ---------------------------------------------------------------------------
// RAG 分块输出（docparse-plan §13 M4）：语义边界优先的检索块
// ---------------------------------------------------------------------------

/// [`to_rag_chunks`] 的默认字符预算。
const DEFAULT_RAG_MAX_CHARS: usize = 1000;
/// chunk 内块文本的连接符（长度计入字符预算）。
const RAG_JOIN: &str = "\n\n";
/// [`RAG_JOIN`] 的字符数（`"\n\n"` 恰是 ASCII，字节数 == 字符数）。
const RAG_JOIN_LEN: usize = 2;

/// RAG 检索块：语义边界优先的文本切片（喂向量化/检索的工作单元）。
#[derive(Debug, Clone, PartialEq)]
pub struct RagChunk {
    /// 稳定锚点 `c{序}`（文档序，从 0 连续递增；块内容不变则重跑结果稳定）
    pub id: String,
    /// 起始块所在页（`Page.name`，无名为页序）。块可跨页续接，此字段记"块头"所在页。
    pub page: Option<String>,
    /// 最近上游 Heading 文本（同 TranslationUnit 口径取"最近一个 Heading"；
    /// 但 chunk 可跨页续接，故按文档序跨页追踪、不随页重置，见 [`to_rag_chunks_with`]）。
    pub chapter: Option<String>,
    /// 块文本：来源块 `Block.content` 原文按文档序以 `\n\n` 连接（不重渲染
    /// Markdown——需渲染态可按 `block_ids` 回查或走 [`to_markdown`]）。
    /// 与 `block_ids` 严格对应：text == 所指各块 content 的依次连接。
    pub text: String,
    /// 来源块锚点（`p{页}-b{块序}`，与 [`TranslationUnit::id`] 同口径）——
    /// 溯源与引用定位用（检索命中回写原文位置、未来双语对齐的基础）。
    pub block_ids: Vec<String>,
}

/// 累加进 chunk 的正文块（规则 ③）。Code/Table/Image 及 Chart/Note 一律不进
/// 检索流（规则 ⑤ 及其推广：结构性/附属内容对向量检索是噪音）。
fn is_rag_body(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Paragraph | BlockKind::Quote | BlockKind::List)
}

/// 按 `DEFAULT_RAG_MAX_CHARS`（1000 字符）把 Document 切成 RAG 检索块。
pub fn to_rag_chunks(doc: &Document) -> Vec<RagChunk> {
    to_rag_chunks_with(doc, DEFAULT_RAG_MAX_CHARS)
}

/// [`to_rag_chunks`] 带自定义字符预算。边界策略（语义边界优先）：
///
/// 1. **绝不跨 Heading**：章边界即块边界（正文绝不与下一章标题同块）；
/// 2. **Heading 总是开新块**（预算未满也切），且标题文本随块入 `text`——
///    检索块自包含（embedding 输入自带章节信号），`chapter` 另供过滤/元数据。
///    连续 Heading（如 H1 紧跟 H2）各自成块（H1 独占标题块），下游可按
///    `block_ids` 自行并块；
/// 3. **Paragraph/Quote/List 逐块累加**：加入后总字符（含连接符）超预算即
///    开新块——贪心装填，每块完整入块、绝不在块内截断；
/// 4. **单块自身超预算不硬切**：保语义完整，允许超长块独占一个 chunk
///    （是否二次切分留给下游按 embedding 模型上限决定）；
/// 5. **Code/Table/Image 不进 chunk**（Chart/Note 同理，见 `is_rag_body`）。
///
/// 与 [`to_translation_units`] 的两处口径差异（均为 chunk 语义所迫）：
/// - **页边界不是块边界**：物理分页无语义，正文跨页续接同一 chunk
///   （`page` 记起始页；跨页块靠 `block_ids` 里的页锚点精确定位）；
/// - **章节上下文跨页追踪**：`chapter` 取块起始处最近的上游 Heading
///   （预算切分产生的续块继承原章；TranslationUnit 因单元不跨页才按页重置）。
///
/// 极端预算（如 `max_chars = 0`）退化为逐块一 chunk；空文档或无正文块的
/// 文档返回空 Vec。
pub fn to_rag_chunks_with(doc: &Document, max_chars: usize) -> Vec<RagChunk> {
    /// 组装中的 chunk（封装口只开 new/push/would_overflow，预算口径不外泄）。
    struct Pending {
        page: Option<String>,
        chapter: Option<String>,
        texts: Vec<String>,
        ids: Vec<String>,
        chars: usize,
    }

    impl Pending {
        fn new(page: String, chapter: Option<String>, text: String, id: String) -> Self {
            Pending {
                chars: text.chars().count(),
                page: Some(page),
                chapter,
                texts: vec![text],
                ids: vec![id],
            }
        }

        fn push(&mut self, text: String, id: String) {
            self.chars += RAG_JOIN_LEN + text.chars().count();
            self.texts.push(text);
            self.ids.push(id);
        }

        fn would_overflow(&self, len: usize, max_chars: usize) -> bool {
            self.chars + RAG_JOIN_LEN + len > max_chars
        }
    }

    let mut chunks: Vec<RagChunk> = Vec::new();
    let mut pending: Option<Pending> = None;
    // 章节上下文：按文档序跨页追踪（见函数级注释）
    let mut chapter: Option<String> = None;

    let close = |pending: &mut Option<Pending>, out: &mut Vec<RagChunk>| {
        if let Some(p) = pending.take() {
            out.push(RagChunk {
                id: format!("c{}", out.len()),
                page: p.page,
                chapter: p.chapter,
                text: p.texts.join(RAG_JOIN),
                block_ids: p.ids,
            });
        }
    };

    for (p_idx, page) in doc.pages.iter().enumerate() {
        let page_name = page.name.clone().unwrap_or_else(|| p_idx.to_string());
        for (b_idx, b) in page.blocks.iter().enumerate() {
            if b.content.trim().is_empty() {
                continue; // 空内容块（含未 OCR 的 Image）无文本可切
            }
            let anchor = format!("p{page_name}-b{b_idx}");
            if b.kind == BlockKind::Heading {
                // 规则 ①②：章边界即块边界，Heading 随文本开新块
                close(&mut pending, &mut chunks);
                chapter = Some(b.content.clone());
                pending = Some(Pending::new(
                    page_name.clone(),
                    chapter.clone(),
                    b.content.clone(),
                    anchor,
                ));
            } else if is_rag_body(b.kind) {
                let len = b.content.chars().count();
                // 规则 ③：累加超预算先封口再装；规则 ④：块本身永不截断
                if matches!(pending.as_ref(), Some(p) if p.would_overflow(len, max_chars)) {
                    close(&mut pending, &mut chunks);
                }
                match &mut pending {
                    Some(p) => p.push(b.content.clone(), anchor),
                    None => {
                        pending = Some(Pending::new(
                            page_name.clone(),
                            chapter.clone(),
                            b.content.clone(),
                            anchor,
                        ))
                    }
                }
            }
            // 其余 kind（Code/Table/Image/Chart/Note）：规则 ⑤，跳过
        }
    }
    close(&mut pending, &mut chunks);
    chunks
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

    fn heading(t: &str) -> Block {
        Block {
            kind: BlockKind::Heading,
            content: t.to_string(),
            level: Some(1),
            ..Default::default()
        }
    }

    fn para(t: &str) -> Block {
        Block {
            kind: BlockKind::Paragraph,
            content: t.to_string(),
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
    fn rag_chunks_split_at_chapter_boundary() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    heading("第一章 夜行"),
                    para("他提着灯走过长街。"),
                    para("巷口的猫叫了一声。"),
                    heading("第二章 晨"),
                    para("天亮了。"),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        // 总字符远低于默认预算 1000，仍必须在章边界切开（规则 ①②）；
        // 标题文本随块入 text，块自包含
        let chunks = to_rag_chunks(&doc);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].id, "c0");
        assert_eq!(chunks[0].chapter.as_deref(), Some("第一章 夜行"));
        assert_eq!(
            chunks[0].text,
            "第一章 夜行\n\n他提着灯走过长街。\n\n巷口的猫叫了一声。"
        );
        assert_eq!(chunks[0].block_ids, ["p0-b0", "p0-b1", "p0-b2"]);
        assert_eq!(chunks[1].id, "c1");
        assert_eq!(chunks[1].chapter.as_deref(), Some("第二章 晨"));
        assert_eq!(chunks[1].text, "第二章 晨\n\n天亮了。");
        assert_eq!(chunks[1].block_ids, ["p0-b3", "p0-b4"]);
    }

    #[test]
    fn rag_chunks_split_on_budget() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    heading("章"),
                    para("aaaaa"),
                    Block {
                        kind: BlockKind::Quote,
                        content: "bbbb".into(),
                        ..Default::default()
                    },
                    list(false, "cccc"),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        // 预算 15：标题(1)+para(5)+quote(4)+连接符×2 = 14 装得下，list 再入(14+2+4=20)即溢出。
        // Paragraph/Quote/List 三类正文块同口径累加
        let chunks = to_rag_chunks_with(&doc, 15);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "章\n\naaaaa\n\nbbbb");
        assert_eq!(chunks[0].text.chars().count(), 14, "贪心装填：不超预算");
        assert_eq!(chunks[0].block_ids, ["p0-b0", "p0-b1", "p0-b2"]);
        // 预算切分出的续块：继承章节上下文；块文本取原文（不重渲染 Markdown）
        assert_eq!(chunks[1].chapter.as_deref(), Some("章"));
        assert_eq!(chunks[1].text, "cccc");
        assert_eq!(chunks[1].block_ids, ["p0-b3"]);
    }

    #[test]
    fn rag_chunk_oversized_block_stays_whole() {
        let big = "甲".repeat(50);
        let doc = Document {
            pages: vec![Page {
                blocks: vec![para(&big), para("尾。")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let chunks = to_rag_chunks_with(&doc, 10);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, big, "单块超预算不硬切，独占一个 chunk（规则 ④）");
        assert_eq!(chunks[0].text.chars().count(), 50);
        assert_eq!(chunks[0].block_ids.len(), 1);
        assert_eq!(chunks[1].text, "尾。");
    }

    #[test]
    fn rag_chunks_exclude_code_table_image() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    para("正文。"),
                    Block {
                        kind: BlockKind::Code,
                        content: "let x = 1;".into(),
                        ..Default::default()
                    },
                    Block {
                        kind: BlockKind::Table,
                        table: Some(crate::model::TableData {
                            rows: vec![vec!["a".into(), "b".into()]],
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    // Image 即使带 OCR 文本也按类型排除（规则 ⑤）
                    Block {
                        kind: BlockKind::Image,
                        content: "图内 OCR 文本".into(),
                        ..Default::default()
                    },
                    para("更多正文。"),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let chunks = to_rag_chunks(&doc);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "正文。\n\n更多正文。");
        // 排除块不占 chunk，但块序号保持原文档索引（锚点不重排）
        assert_eq!(chunks[0].block_ids, ["p0-b0", "p0-b4"]);
    }

    #[test]
    fn rag_chunks_empty_document() {
        assert!(to_rag_chunks(&Document::default()).is_empty());
        // 有页但无正文块（仅排除类型 / 空白内容）：同样无 chunk
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    Block {
                        kind: BlockKind::Code,
                        content: "fn main() {}".into(),
                        ..Default::default()
                    },
                    para("   "),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(to_rag_chunks(&doc).is_empty());
    }

    #[test]
    fn rag_chunks_block_ids_trace_provenance() {
        let long = "长".repeat(30);
        let doc = Document {
            pages: vec![
                Page {
                    // 无名页 → 页序 "0"（与 TranslationUnit 同口径）
                    blocks: vec![heading("导论"), para("本书讨论文档解析。"), para(&long)],
                    ..Default::default()
                },
                Page {
                    name: Some("9".into()),
                    blocks: vec![para("跨页续段。"), para("再一段。")],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let chunks = to_rag_chunks_with(&doc, 40);
        assert_eq!(
            chunks.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["c0", "c1", "c2"],
            "id 按文档序连续编号"
        );
        // c1 由预算切分产生且跨页续接：page 记起始页，chapter 跨页继承
        assert_eq!(chunks[1].page.as_deref(), Some("0"));
        assert_eq!(chunks[1].block_ids, ["p0-b2", "p9-b0"]);
        assert_eq!(chunks[1].chapter.as_deref(), Some("导论"));
        assert_eq!(chunks[2].page.as_deref(), Some("9"));
        assert_eq!(chunks[2].chapter.as_deref(), Some("导论"));

        // 溯源不变式：text 恒等于 block_ids 所指块 content 的依次连接
        let mut by_id = std::collections::HashMap::new();
        for (p_idx, page) in doc.pages.iter().enumerate() {
            let name = page.name.clone().unwrap_or_else(|| p_idx.to_string());
            for (b_idx, b) in page.blocks.iter().enumerate() {
                by_id.insert(format!("p{name}-b{b_idx}"), b.content.clone());
            }
        }
        for c in &chunks {
            let rebuilt = c
                .block_ids
                .iter()
                .map(|id| by_id[id].as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            assert_eq!(c.text, rebuilt, "chunk {}", c.id);
        }
    }

    #[test]
    fn rag_chunks_consecutive_headings_split() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    heading("第一部分"),
                    heading("第一章 夜行"),
                    para("他提着灯走过长街。"),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        // 规则 ② 从严：每个 Heading 都开新块——H1 独占标题块，H2 与正文同块
        let chunks = to_rag_chunks(&doc);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "第一部分");
        assert_eq!(chunks[0].block_ids, ["p0-b0"]);
        assert_eq!(chunks[1].text, "第一章 夜行\n\n他提着灯走过长街。");
        assert_eq!(chunks[1].block_ids, ["p0-b1", "p0-b2"]);
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
