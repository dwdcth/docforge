// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① Image 块渲染 [image] 占位；② feature `json` 下 to_json/
// to_json_pretty；③ 新增 to_translation_units（翻译友好输出，docparse-plan §9）；
// ④ 新增 to_rag_chunks（RAG 检索分块，docparse-plan §13 M4）；
// ⑤ 新增双语回写（BiStrategy + to_bilingual_markdown[_with]，翻译闭环①）；
// ⑥ 表格进翻译流：Table 块按单元格展开（`-r{行}c{列}` 锚点 + header 表头
//    标志），双语回写按译后网格渲染。
//! 输出目标：text / markdown / JSON（feature `json`）/ 翻译单元
//!（`to_translation_units`，翻译管线的工作单元与回写锚点）/ RAG 检索块
//!（`to_rag_chunks`，向量化/检索管线的工作单元）/ 双语回写
//!（`to_bilingual_markdown`，按锚点把译文写回 Markdown）。

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

/// Markdown 表格格转义：裸 `|` 会撕裂表格结构（原文渲染与译文注入统一走这里，
/// 译文里出现竖线时网格不被冲垮）。
fn escape_md_cell(s: &str) -> String {
    s.replace('|', "\\|")
}

fn table_md(rows: &[Vec<String>]) -> String {
    // 单元格统一过转义（现状未转义是缺陷：原文格含 `|` 时表格同样会被撕裂）
    let escaped: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| escape_md_cell(c)).collect())
        .collect();
    table_rows_md(&escaped)
}

/// 按已就绪（已转义）的单元格网格渲染 Markdown 表格：补齐短行、首行 +
/// 分隔行。[`table_md`]（原文渲染）与双语回写的译后网格共用。
fn table_rows_md(rows: &[Vec<String>]) -> String {
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
    /// 稳定 ID：块单元 `p{页号}-b{块序}`（页号取 Page.name，无名为页序）；
    /// Table 展开的格单元 `p{页号}-b{块序}-r{行}c{列}`（见
    /// [`to_translation_units_with`]）
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
    /// 表头单元格（Table 块展开且行号 < `TableData.header_rows`）；非表格单元
    /// 恒 false。消费者可按此跳过表头翻译。
    pub header: bool,
    /// 跨页重复的页眉/页脚类内容（`Block.meta["running"]` 的事实复制，检测
    /// 见 `plugins::running_detect`）：同文本以近同位置出现在 ≥3 个不同页上。
    /// 消费者可跳过送译或按文本键合并翻一次；**单元照发**（锚点契约不动）。
    /// 表格格单元继承所在块的标志（表格自身跨页重复罕见，继承无害）。
    pub running: bool,
}

impl TranslationUnit {
    /// 是否为 OCR 识别文本（翻译前可按置信度决定人工复核）。
    pub fn is_ocr(&self) -> bool {
        self.source == BlockSource::Ocr
    }

    /// 单元文本是否含可翻译的自然语言（[`has_translatable_text`] 的便捷
    /// 委托）——翻译管线送译前的防幻觉过滤：`220V` / `-10 to 50 C` 这类
    /// 纯数字/单位文本送引擎只会得到 "220V 电话" 式幻觉，不是漏翻。
    pub fn has_translatable_text(&self) -> bool {
        has_translatable_text(&self.text)
    }
}

/// 译文对：id 锚点 + 译文 + 可选的**①侧源文本**（自校验锚点）。
///
/// 回写管线两处解析（① 消费者提取译文依据 / ④ 回写器重解析建立块序）
/// 之间文件可能被修改——位置锚点在新文件里会指向**别的文本**，而回写器
/// 内部的"槽位 ↔ 单元"守卫两侧都来自 ④，拦不住这种漂移（静默错位写，
/// 回写族的最恶劣故障模式）。`source: Some` 把 ① 侧源文本随译文带上：
/// 回写器写入前校验它与当前（④）文本逐字一致，不一致跳过该单元并在
/// `WritebackReport` 记因（宁漏勿错）。
///
/// `None` = 信任锚点（现行为）；`(id, 译文)` 二元组 `From` 即此形态，
/// `(id, 源文本, 译文)` 三元组 `From` 为带校验形态。
#[derive(Debug, Clone, PartialEq)]
pub struct TranslationPair {
    /// [`TranslationUnit::id`]（回写锚点）。
    pub id: String,
    /// 译文（空白视为未提供，与回写既有口径一致）。
    pub translation: String,
    /// 提取译文时依据的源文本（① 侧）；`None` = 不校验。
    pub source: Option<String>,
}

impl From<(String, String)> for TranslationPair {
    fn from((id, translation): (String, String)) -> Self {
        TranslationPair { id, translation, source: None }
    }
}

impl From<(String, String, String)> for TranslationPair {
    /// `(id, 源文本, 译文)`——带源校验的形态。
    fn from((id, source, translation): (String, String, String)) -> Self {
        TranslationPair { id, translation, source: Some(source) }
    }
}

/// 文本是否含**可翻译的自然语言**（事实谓词；策略归消费者）。
///
/// 这是三级"什么值得翻译"判据的第三级事实：块类白名单（[`is_translatable`]）
/// → 噪声块（`min_chars`）→ **语言性**（本谓词）。判定规则（保守偏向）：
///
/// - 任一**非 ASCII 字母**（CJK / 西里尔等）→ `true`（几乎必是自然语言）；
/// - 存在 **≥3 连续 ASCII 字母**的词 → `true`（"hello"、"Version 2 released"；
///   "mod"/"err" 这类短代码词会被判 true——**宁多送不漏翻**：漏翻=永远不翻，
///   多送=最坏一次幻觉，且消费者可再过滤）；
/// - 其余 → `false`：纯数字/符号/短单位串——`220`、`220V`、`1.2.3`、`v2.0`、
///   `±5%`、`-10 to 50 C`（连接词/单位均 ≤2 字母）、`50 Hz`、`5V2A`、
///   `2024-01-01`、`#FF00AA`。非字母符号（`℃` 等）不触发 true。
///
/// **只判事实，不做策略**：跳过送译还是仍送（如医疗场景 "50 mg"→"50 毫克"
/// 的单位本地化）由消费者决定；本谓词也不改变 [`to_translation_units`]
/// 的输出——单元照发（锚点契约不动），消费者按谓词过滤送译清单即可，
/// 回写侧"未命中 = 原样"天然组合。rumi docmt 的送译前过滤建议直接复用，
/// 避免双侧正则口径漂移。
pub fn has_translatable_text(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    // 非 ASCII 字母（CJK/西里尔/带变音符拉丁等）一票判定
    if t.chars().any(|c| c.is_alphabetic() && !c.is_ascii()) {
        return true;
    }
    // 逐个极大 ASCII 字母 run：出现 ≥3 连字母即视为自然语言词
    let mut run = 0usize;
    for c in t.chars() {
        if c.is_ascii_alphabetic() {
            run += 1;
            if run >= 3 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    // 无任何 ≥3 字母 run：纯数字/符号/短单位串（阈值 3 由实测痛点样本定：
    // "-10 to 50 C" 的连接词 to=2、单位 V/Hz/C ≤2 都必须判 false）
    false
}

/// 可翻译块白名单：Heading / Paragraph / Quote / List（Code/Note 等结构性
/// 内容不进翻译流）。
///
/// **Table 不在本谓词——刻意不翻转**：块级 `content` 只是表头摘要，且六个
/// 回写器（`writeback/*`）以内联复刻的本谓词做「第 k 单元 ↔ 第 k XML 位」
/// 对齐，翻转会让提取侧单元流与回写侧位置序立即失控。语义等价承载：表格经
/// [`to_translation_units`] 按单元格展开，单元 `kind = BlockKind::Table`
/// 标识、`header` 标识表头行，外部消费者据此识别与过滤。
pub fn is_translatable(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Heading | BlockKind::Paragraph | BlockKind::Quote | BlockKind::List)
}

/// [`to_translation_units`] 的默认最小字符数（提取与回写两侧共用同一口径）。
const DEFAULT_TRANSLATION_MIN_CHARS: usize = 2;

/// 块的跨页重复事实：`Block.meta["running"] = Bool(true)`（检测见
/// `plugins::running_detect`）。只认显式 `Bool(true)`，其余取值（含缺键）
/// 一律 false。
fn block_running(b: &crate::model::Block) -> bool {
    matches!(
        b.meta.get("running"),
        Some(crate::model::MetaValue::Bool(true))
    )
}

/// 提取/回写两侧共用的块锚点：`p{页名}-b{块序}`。单点定义，消除两处
/// format! 的口径漂移——锚点对不上译文就写不回。
fn translation_id(page_name: &str, b_idx: usize) -> String {
    format!("p{page_name}-b{b_idx}")
}

/// 提取/回写两侧共用的单元格锚点：`p{页名}-b{块序}-r{行}c{列}`（如
/// `p0-b2-r1c3`）。与 [`translation_id`] 并列单点定义——id 是回写锚点，
/// 绝不允许两处 format! 口径漂移。与块锚点字符串形态不同（`p0-b2` 与
/// `p0-b2-r1c3`），天然不冲突。
fn translation_cell_id(page_name: &str, b_idx: usize, r: usize, c: usize) -> String {
    format!("p{page_name}-b{b_idx}-r{r}c{c}")
}

/// 提取/回写两侧共用的候选判定：可翻译白名单 + 非空白 + 达到 min_chars。
/// 直接复用 [`is_translatable`]；回写侧用同一谓词，保证「哪些块拥有锚点」
/// 两侧完全一致（id 对得上）。
fn translation_candidate(b: &crate::model::Block, min_chars: usize) -> bool {
    is_translatable(b.kind)
        && !b.content.trim().is_empty()
        && b.content.chars().count() >= min_chars
}

/// 提取翻译单元：按文档序遍历块，白名单 + 最小字符数过滤；Table 块按
/// 单元格展开进流（锚点 `p{页}-b{块序}-r{行}c{列}`，见
/// [`to_translation_units_with`]）。
///
/// `min_chars` 过滤超短块（默认 2：滤掉单字符噪声，保留绝大多数实词句）。
/// 章节上下文 = 该块之前最近的 Heading 块文本（跨页不追踪）。
pub fn to_translation_units(doc: &Document) -> Vec<TranslationUnit> {
    to_translation_units_with(doc, DEFAULT_TRANSLATION_MIN_CHARS)
}

/// [`to_translation_units`] 带自定义最小字符数。
///
/// **表格单元格不受 `min_chars` 约束**：`cell.trim()` 非空即收（阈值恒 1）。
/// 单字符格（是/否、单位 V/A、币种符号）是表格常态，min_chars=2 会把参数表
/// 洗成空壳；数字/纯符号格翻译引擎自然原样返回，无害。表头行（行号 <
/// `TableData.header_rows`）的单元 `header = true`，消费者可按此跳过表头
/// 翻译；单元 `kind = BlockKind::Table`，外部消费者据此识别单元格单元。
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
            // 表格块按单元格展开进翻译流（此前表格文本完全不进翻译流——
            // rumi 等「整份文档翻译」消费者拿不到参数表/对照表/数据字典）。
            // 无 rows 结构的 Table 块防御性跳过（理论不出现：各解析后端
            // 产 Table 块必带 table）。
            if b.kind == BlockKind::Table {
                let Some(t) = b.table.as_ref() else {
                    continue;
                };
                for (r, row) in t.rows.iter().enumerate() {
                    for (c, cell) in row.iter().enumerate() {
                        let text = cell.trim();
                        if text.is_empty() {
                            continue; // 空格不发单元（合并格/留白格常态）
                        }
                        out.push(TranslationUnit {
                            id: translation_cell_id(&page_name, b_idx, r, c),
                            page: Some(page_name.clone()),
                            kind: BlockKind::Table,
                            text: text.to_string(),
                            source: b.source,
                            confidence: b.confidence,
                            lang: b.lang.clone(),
                            chapter: chapter.clone(),
                            header: r < t.header_rows,
                            running: block_running(b),
                        });
                    }
                }
                continue;
            }
            if !translation_candidate(b, min_chars) {
                continue;
            }
            out.push(TranslationUnit {
                id: translation_id(&page_name, b_idx),
                page: Some(page_name.clone()),
                kind: b.kind,
                text: b.content.clone(),
                source: b.source,
                confidence: b.confidence,
                lang: b.lang.clone(),
                chapter: chapter.clone(),
                header: false,
                running: block_running(b),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 双语回写（翻译闭环 ①）：按 TranslationUnit.id 锚点把译文写回 Markdown
// ---------------------------------------------------------------------------

/// 双语回写策略（Markdown 形态；docx/epub 回写器共用同一套语义）。
///
/// 三种策略共用 [`to_translation_units`] 的锚点与过滤口径（页×块序、
/// [`is_translatable`] 白名单、min_chars 2；Table 块另发单元格锚点
/// `p{页}-b{块序}-r{行}c{列}`）：只有提取侧会发锚点的块/格才可能被译文
/// 命中；无译文的块一律保留原文；未匹配的译文 id 忽略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BiStrategy {
    /// 原/译同段交错（原文块后紧跟译文块）——阅读对照形态。
    ///
    /// 译文与原文之间不空行（`\n` 直连，Markdown 渲染为同段软换行）：对照
    /// 紧凑是双语对照读物的主流形态。em 级弱化（斜体标译文）刻意不做——
    /// Markdown 形态保持纯文本，译文不借用原文的强调语义。
    #[default]
    BilingualInline,
    /// 仅译文（替换原文块文本）。
    TranslationOnly,
    /// 原文完整 + 文末追加全部译文附录（按 chapter 分组）。
    OriginalPlusAppendix,
}

/// 译文块的 Markdown 形态：Heading 保持同级标题（层级沿用原块），其余一律
/// 纯文本——译文即正文，不加列表/引用前缀（translator 翻的就是块 content
/// 原文，回写按文本对文本）。首尾空白裁剪：翻译端常见的尾换行不破坏
/// 「不空行」的对照形态。
fn translation_md(b: &crate::model::Block, translation: &str) -> String {
    match b.kind {
        BlockKind::Heading => {
            let lvl = (b.level.unwrap_or(1) as usize).clamp(1, 6);
            format!("{} {}", "#".repeat(lvl), translation.trim())
        }
        _ => translation.trim().to_string(),
    }
}

/// 双语 Markdown 回写：`pairs` 的 id 对应 [`to_translation_units`] 的 id
/// 锚点（`p{页}-b{块序}`）；无译文的块按策略保留原文。未匹配的译文 id 忽略。
/// 默认 [`BiStrategy::BilingualInline`]；见 [`to_bilingual_markdown_with`]。
pub fn to_bilingual_markdown(doc: &Document, pairs: &[TranslationPair]) -> String {
    to_bilingual_markdown_with(doc, pairs, BiStrategy::default())
}

/// [`to_bilingual_markdown`] 带策略。
///
/// 遍历与过滤口径与 [`to_translation_units`] 完全一致（页×块序；
/// [`is_translatable`] 白名单 + 非空白 + min_chars 2）——提取/回写两侧共用
/// 同一锚点函数与同一候选谓词，id 不会漂移。非翻译块（Code/Table/Image 等）
/// 原样按块渲染保留。
///
/// 各策略形态：
///
/// - [`BiStrategy::BilingualInline`]：原文渲染后 `\n` 直连译文（不空行，
///   同段对照）；Heading 译文保持同级标题；Table 按格对照（格内
///   `原文<br>译文`，Markdown 表格内换行本就用 `<br>`）；
/// - [`BiStrategy::TranslationOnly`]：译文整体替换原块文本（Heading 保持
///   层级，其余纯文本）；Table 全命中格替换为译文；
/// - [`BiStrategy::OriginalPlusAppendix`]：正文为 [`to_markdown`] 完整原文 +
///   `\n\n---\n\n# 译文\n\n`，译文按 chapter 分组（`##` 组标题，保持章节
///   首次出现序；chapter 为 None 归 `（无章节）`），组内按文档序以空行分段。
///   附录内译文一律纯文本段落（标题层级归附录自身的分组结构）。**表格单元
///   不进附录**（表格格译文无处安放——附录是段落流），正文表格原样渲染。
///   全文无命中译文时不追加空附录（输出即 [`to_markdown`]）。
///
/// 与 [`to_markdown`] 的渲染差异：双语遍历不做相邻列表合并（锚点对齐优先，
/// 列表项逐条独立）；List/Quote 的译文为纯文本，Markdown 渲染中作为前文
/// 续行/正文出现；Table 块按译后网格渲染（单元格锚点逐格查译文，无任何
/// 命中格时输出与 [`to_markdown`] 的表格渲染逐字节一致）。
///
/// `TranslationPair::source` 在此**忽略**（md 无重解析，锚点与 doc 同对象）：
/// 回写依据与输入 `doc` 是同一份块序，① 提取与 ④ 回写之间不存在文件变更
/// 边界，源校验无从谈起——二元组与三元组（含不一致源）形态行为一致。
pub fn to_bilingual_markdown_with(
    doc: &Document,
    pairs: &[TranslationPair],
    strategy: BiStrategy,
) -> String {
    // md 无重解析，锚点与 doc 同对象：忽略 source（二元组/三元组形态一致）
    let map: std::collections::HashMap<&str, &str> = pairs
        .iter()
        .map(|p| (p.id.as_str(), p.translation.as_str()))
        .collect();

    if strategy == BiStrategy::OriginalPlusAppendix {
        return original_plus_appendix(doc, &map);
    }

    let mut out: Vec<String> = Vec::new();
    for (p_idx, page) in doc.pages.iter().enumerate() {
        let page_name = page.name.clone().unwrap_or_else(|| p_idx.to_string());
        for (b_idx, b) in page.blocks.iter().enumerate() {
            // 表格块：按单元格锚点查译文，渲染译后网格（与提取侧展开口径
            // 同源——translation_cell_id 单点定义，两侧 id 不漂移）。无结构
            // 的 Table 块与 to_markdown 同口径渲染为空、跳过。
            if b.kind == BlockKind::Table {
                let Some(t) = b.table.as_ref() else {
                    continue;
                };
                let grid: Vec<Vec<String>> = t
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(r, row)| {
                        row.iter()
                            .enumerate()
                            .map(|(c, cell)| {
                                // 原格与 table_md 同口径（不 trim，只转义）：
                                // 无命中格的表格与 to_markdown 输出逐字节一致
                                let orig = escape_md_cell(cell);
                                match map
                                    .get(translation_cell_id(&page_name, b_idx, r, c).as_str())
                                {
                                    None => orig,
                                    Some(tr) => match strategy {
                                        BiStrategy::TranslationOnly => escape_md_cell(tr.trim()),
                                        BiStrategy::BilingualInline => {
                                            format!("{orig}<br>{}", escape_md_cell(tr.trim()))
                                        }
                                        BiStrategy::OriginalPlusAppendix => {
                                            unreachable!("已在分支前提前返回")
                                        }
                                    },
                                }
                            })
                            .collect()
                    })
                    .collect();
                let rendered = table_rows_md(&grid);
                if !rendered.is_empty() {
                    out.push(rendered);
                }
                continue;
            }
            let rendered = block_md(b);
            if rendered.is_empty() {
                continue;
            }
            let translation = if translation_candidate(b, DEFAULT_TRANSLATION_MIN_CHARS) {
                map.get(translation_id(&page_name, b_idx).as_str()).copied()
            } else {
                None
            };
            match translation {
                None => out.push(rendered),
                Some(t) => match strategy {
                    BiStrategy::BilingualInline => {
                        out.push(format!("{rendered}\n{}", translation_md(b, t)));
                    }
                    BiStrategy::TranslationOnly => out.push(translation_md(b, t)),
                    BiStrategy::OriginalPlusAppendix => unreachable!("已在分支前提前返回"),
                },
            }
        }
    }
    out.join("\n\n")
}

/// [`BiStrategy::OriginalPlusAppendix`] 的输出形态：完整原文 + 按章分组附录。
fn original_plus_appendix(doc: &Document, map: &std::collections::HashMap<&str, &str>) -> String {
    let body = to_markdown(doc);
    // (chapter, 该章译文们)：保持章节首次出现序（遍历即文档序）
    let mut groups: Vec<(Option<String>, Vec<String>)> = Vec::new();
    for (p_idx, page) in doc.pages.iter().enumerate() {
        let page_name = page.name.clone().unwrap_or_else(|| p_idx.to_string());
        // 章节追踪与提取侧同口径：每个 Heading 都更新（含被 min_chars 滤掉的
        // 短标题，其自身章节即自身文本）；跨页不追踪（随页重置）
        let mut chapter: Option<String> = None;
        for (b_idx, b) in page.blocks.iter().enumerate() {
            if b.kind == BlockKind::Heading {
                chapter = Some(b.content.clone());
            }
            // 表格单元格单元（kind = Table）不在 translation_candidate 白名单
            // 内，结构性地不进附录（附录是段落流，格译文无处安放；表格正文
            // 由 body = to_markdown 原样渲染）；running（跨页重复页眉/页脚类）
            // 同样不进——与 writeback::appendix::to_appendix_pages 的默认口径
            // 一致（页眉译 N 遍灌满附录正是 running 检测的痛点场景）
            if !translation_candidate(b, DEFAULT_TRANSLATION_MIN_CHARS) || block_running(b) {
                continue;
            }
            let t = match map.get(translation_id(&page_name, b_idx).as_str()).copied() {
                Some(t) => t,
                None => continue,
            };
            match groups
                .iter_mut()
                .find(|(c, _)| c.as_deref() == chapter.as_deref())
            {
                Some((_, texts)) => texts.push(t.trim().to_string()),
                None => groups.push((chapter.clone(), vec![t.trim().to_string()])),
            }
        }
    }
    if groups.is_empty() {
        return body; // 无命中译文：不追加空附录
    }
    let mut appendix: Vec<String> = vec!["---".into(), "# 译文".into()];
    for (chapter, texts) in groups {
        appendix.push(format!(
            "## {}\n\n{}",
            chapter.as_deref().unwrap_or("（无章节）"),
            texts.join("\n\n")
        ));
    }
    format!("{body}\n\n{}", appendix.join("\n\n"))
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
            let anchor = translation_id(&page_name, b_idx);
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

    // -------------------------------------------------------------------------
    // 语言性谓词（防幻觉过滤的事实层，doc 注释见 has_translatable_text）
    // -------------------------------------------------------------------------

    /// 痛点样本全集：送引擎只会得到 "220V 电话" 式幻觉的文本，一律 false。
    #[test]
    fn has_translatable_text_filters_numeric_symbol_units() {
        for t in [
            "220", "220V", "1.2.3", "v2.0", "±5%", "-10 to 50 C", "50 Hz", "5V2A",
            "2024-01-01", "10:30", "#FF00AA", "A1", "x", "±5%℃", "", "   ",
        ] {
            assert!(!has_translatable_text(t), "{t:?} 应判 false");
        }
    }

    /// 自然语言一律 true；短代码词（mod/err）也 true——宁多送不漏翻
    ///（漏翻=永远不翻，多送=最坏一次幻觉且消费者可再过滤）。
    #[test]
    fn has_translatable_text_passes_natural_language() {
        for t in [
            "你好", "第2章", "hello world", "Version 2 released", "UTF-8 编码",
            "OK 了", "额定电压与频率", "mod",
        ] {
            assert!(has_translatable_text(t), "{t:?} 应判 true");
        }
    }

    /// 单元照发（锚点契约不动）+ 方法委托：纯数字格仍是翻译单元，
    /// 消费者按谓词过滤**送译清单**——回写侧"未命中 = 原样"天然组合。
    #[test]
    fn units_still_emitted_for_numeric_text_with_predicate_available() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![Block {
                    kind: BlockKind::Table,
                    table: Some(crate::model::TableData {
                        rows: vec![vec!["参数".into(), "220V".into()]],
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 2, "纯数字格照发单元（锚点契约不动）");
        assert!(units[0].has_translatable_text(), "「参数」是自然语言");
        assert!(!units[1].has_translatable_text(), "「220V」由消费者过滤送译");
    }

    /// running（跨页重复页眉/页脚类）块：单元照发（`running = true` 供消费
    /// 者过滤），但 Markdown 附录与 writeback::appendix 同口径默认不收——
    /// 页眉译 N 遍灌满附录正是 running 检测的痛点场景。
    #[test]
    fn running_blocks_emit_units_but_skip_markdown_appendix() {
        let mut meta = std::collections::BTreeMap::new();
        meta.insert("running".to_string(), true.into());
        let running = Block {
            kind: BlockKind::Paragraph,
            content: "技术白皮书 V3".into(),
            meta,
            ..Default::default()
        };
        let doc = Document {
            pages: vec![Page {
                blocks: vec![running, para("正文一段。")],
                ..Default::default()
            }],
            ..Default::default()
        };
        // 单元照发：running 单元仍在流里（锚点契约不动），带事实标志
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 2);
        assert!(units[0].running, "页眉类块带 running 标志");
        assert!(!units[1].running);

        // 附录只收正文：页眉译文不进（与 to_appendix_pages 同口径）
        let translated = vec![
            ("p0-b0".to_string(), "Technical Whitepaper V3".to_string()).into(),
            ("p0-b1".to_string(), "Body paragraph.".to_string()).into(),
        ];
        let md = to_bilingual_markdown_with(&doc, &translated, BiStrategy::OriginalPlusAppendix);
        assert!(md.contains("# 译文"), "有命中译文，附录存在");
        assert!(
            !md.contains("Technical Whitepaper V3"),
            "running 单元的译文不进附录"
        );
        assert!(md.contains("Body paragraph."), "正文单元照收");
    }

    // -------------------------------------------------------------------------
    // 表格进翻译流（提案 §3）：Table 块按单元格展开
    // -------------------------------------------------------------------------

    /// 带行列结构与表头行数的 Table 块（测试构造件）。
    fn table(rows: &[&[&str]], header_rows: usize) -> Block {
        Block {
            kind: BlockKind::Table,
            table: Some(crate::model::TableData {
                rows: rows
                    .iter()
                    .map(|r| r.iter().map(|s| s.to_string()).collect())
                    .collect(),
                header_rows,
            }),
            ..Default::default()
        }
    }

    /// 单元展开：混合文档（段落 + header_rows=1 的 3×3 表）——单元数、
    /// id 形态（`p0-b2-r0c0`…）、header 标志（首行 true 其余 false）、
    /// 单字符格与空格格口径、min_chars 只约束段落类。
    #[test]
    fn table_units_expand_by_cell() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    heading("附录 参数表"),   // b0：章节上下文
                    para("如下表。"),         // b1：4 字符，默认口径进流、min_chars=5 被滤
                    table(                    // b2：3×3，表头 1 行，r2c0 为空格
                        &[&["名称", "单位", "默认"], &["电压", "V", "是"], &["", "A", "电流"]],
                        1,
                    ),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let units = to_translation_units(&doc);
        // 2 个段落类单元 + 8 个非空格（3×3 减 1 个空格格）
        assert_eq!(units.len(), 2 + 8);
        // id 形态：块锚点与单元格锚点同流且不冲突（`p0-b2` 与 `p0-b2-r0c0`
        // 字符串不同）；空格格 r2c0 不发单元
        assert_eq!(
            units.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            [
                "p0-b0",
                "p0-b1",
                "p0-b2-r0c0",
                "p0-b2-r0c1",
                "p0-b2-r0c2",
                "p0-b2-r1c0",
                "p0-b2-r1c1",
                "p0-b2-r1c2",
                "p0-b2-r2c1",
                "p0-b2-r2c2",
            ]
        );
        // header 标志：行号 < header_rows(1) → 首行 true、其余行 false；
        // 段落类单元恒 false
        assert!(
            units.iter().all(|u| match u.id.as_str() {
                "p0-b2-r0c0" | "p0-b2-r0c1" | "p0-b2-r0c2" => u.header,
                _ => !u.header,
            }),
            "首行格 header=true，其余单元 header=false"
        );
        // 格阈值恒 1：单字符格 "V"/"A"/"是" 在默认 min_chars=2 下仍进流
        for (id, text) in [("p0-b2-r1c1", "V"), ("p0-b2-r1c2", "是"), ("p0-b2-r2c1", "A")] {
            let u = units.iter().find(|u| u.id == id).unwrap();
            assert_eq!(u.text, text, "单字符格 {text} 不受 min_chars 洗刷");
        }
        // min_chars 只约束段落类：min_chars=5 时 4 字符段落被滤，8 个格照收
        let units5 = to_translation_units_with(&doc, 5);
        assert_eq!(units5.iter().filter(|u| u.kind == BlockKind::Table).count(), 8);
        assert!(!units5.iter().any(|u| u.id == "p0-b1"), "段落按 min_chars=5 过滤");
        // 单元字段承自所在块；章节上下文继承所在位置
        let cell = units.iter().find(|u| u.id == "p0-b2-r1c0").unwrap();
        assert_eq!(cell.kind, BlockKind::Table);
        assert_eq!(cell.text, "电压");
        assert_eq!(cell.chapter.as_deref(), Some("附录 参数表"));
        assert_eq!(cell.page.as_deref(), Some("0"));
    }

    /// 无 rows 结构的 Table 块防御性跳过（块级 content 只是表头摘要，不发单元）。
    #[test]
    fn table_block_without_data_stays_out() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![Block {
                    kind: BlockKind::Table,
                    content: "参数速览（表头摘要）".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(to_translation_units(&doc).is_empty());
    }

    // -------------------------------------------------------------------------
    // 跨页重复（running）单元：`Block.meta["running"]` 的事实复制
    //（检测侧见 plugins::running_detect；此处只验 emit 层的复制与继承）
    // -------------------------------------------------------------------------

    /// 段落单元承所在块的 running 标志；未标记文档零变化（running 恒 false）。
    #[test]
    fn units_carry_running_flag() {
        let mut marked = para("季度运营报告");
        marked
            .meta
            .insert("running".into(), crate::model::MetaValue::Bool(true));
        marked
            .meta
            .insert("region".into(), "header".into());
        let doc = Document {
            pages: vec![Page {
                blocks: vec![marked, para("正文一段。")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 2);
        assert!(units[0].running, "标记块的单元 running=true");
        assert!(!units[1].running, "未标记块的单元 running=false");
        // region 是检测侧细标，单元层不复制（running 单元文本键可自行归并）
        assert_eq!(units[0].text, "季度运营报告");

        // 零变化回归：不带任何 meta 的普通文档 running 恒 false
        let plain = to_translation_units(&Document {
            pages: vec![Page { blocks: vec![para("正文一段。")], ..Default::default() }],
            ..Default::default()
        });
        assert!(plain.iter().all(|u| !u.running));
    }

    /// 格单元继承所在块的 running 标志（表格自身跨页重复罕见，继承无害）。
    #[test]
    fn table_cell_units_inherit_running() {
        let mut marked_table = table(&[&["表头甲", "表头乙"], &["格一", "格二"]], 1);
        marked_table
            .meta
            .insert("running".into(), crate::model::MetaValue::Bool(true));
        let doc = Document {
            pages: vec![Page { blocks: vec![marked_table], ..Default::default() }],
            ..Default::default()
        };
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 4);
        assert!(units.iter().all(|u| u.running), "全部格单元继承块级 running");
        // 既有标志不串位：header 语义独立并存
        assert!(units.iter().take(2).all(|u| u.header) && units.iter().skip(2).all(|u| !u.header));
    }

    /// 仅译文：表格全命中格替换为译文，网格结构（行数/列数/分隔行）不变。
    #[test]
    fn bilingual_table_translation_only() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![table(&[&["表头甲", "表头乙"], &["格一", "格二"]], 0)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let translated: Vec<TranslationPair> = to_translation_units(&doc)
            .iter()
            .map(|u| (u.id.clone(), format!("译{}", u.text)).into())
            .collect();
        let md = to_bilingual_markdown_with(&doc, &translated, BiStrategy::TranslationOnly);
        assert_eq!(md, "| 译表头甲 | 译表头乙 |\n| --- | --- |\n| 译格一 | 译格二 |");
        assert!(!md.contains("| 格一 |"), "原格文本被替换（译格是另一格值）");
    }

    /// 原/译对照：格内 `原文<br>译文`（Markdown 表格内换行本就用 `<br>`）。
    #[test]
    fn bilingual_table_inline_br() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![table(&[&["表头甲", "表头乙"], &["格一", "格二"]], 0)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let translated: Vec<TranslationPair> = to_translation_units(&doc)
            .iter()
            .map(|u| (u.id.clone(), format!("译{}", u.text)).into())
            .collect();
        assert_eq!(
            to_bilingual_markdown(&doc, &translated),
            "| 表头甲<br>译表头甲 | 表头乙<br>译表头乙 |\n| --- | --- |\n| 格一<br>译格一 | 格二<br>译格二 |"
        );
    }

    /// 原文+附录：表格单元不进附录，正文表格原样渲染（原文格保留）。
    #[test]
    fn bilingual_table_appendix_excludes_cells() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![para("正文一段。"), table(&[&["表头甲", "表头乙"], &["格一", "格二"]], 0)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let translated: Vec<TranslationPair> = to_translation_units(&doc)
            .iter()
            .map(|u| (u.id.clone(), format!("译{}", u.text)).into())
            .collect();
        let md = to_bilingual_markdown_with(&doc, &translated, BiStrategy::OriginalPlusAppendix);
        // 正文 = to_markdown 原样（表格原文保留）；附录只收段落类译文
        assert_eq!(
            md,
            format!(
                "{}\n\n---\n\n# 译文\n\n## （无章节）\n\n译正文一段。",
                to_markdown(&doc)
            )
        );
        assert!(md.contains("| 表头甲 | 表头乙 |"), "正文表格原样");
        assert!(!md.contains("译表头甲") && !md.contains("译格一"), "格译文不进附录");
    }

    /// 无任何命中格的表格：输出与 to_markdown 的表格渲染逐字节一致（回归保护）。
    #[test]
    fn bilingual_table_without_hits_unchanged() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![para("正文一段。"), table(&[&["表头甲", "表头乙"], &["格一", "格二"]], 0)],
                ..Default::default()
            }],
            ..Default::default()
        };
        // 只命中段落，表格格全部未命中（默认 BilingualInline：段内对照）
        let translated = vec![("p0-b0".to_string(), "译正文一段。".to_string()).into()];
        let md = to_bilingual_markdown(&doc, &translated);
        assert_eq!(
            md,
            format!("正文一段。\n译正文一段。\n\n{}", to_markdown(&doc).split("\n\n").last().unwrap())
        );
    }

    /// 竖线转义：原格与译格里的裸 `|` 一律渲染为 `\|`（否则撕裂表格结构）；
    /// to_markdown 的原文渲染同口径修正。
    #[test]
    fn table_cell_pipe_escaped() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![table(&[&["a|b", "c"], &["d", "e"]], 0)],
                ..Default::default()
            }],
            ..Default::default()
        };
        // 原文渲染：原格转义
        assert_eq!(
            to_markdown(&doc),
            "| a\\|b | c |\n| --- | --- |\n| d | e |"
        );
        // 仅译文：译格 r0c1 含竖线 → 转义后注入
        let translated = vec![("p0-b0-r0c1".to_string(), "x|y".to_string()).into()];
        let md = to_bilingual_markdown_with(&doc, &translated, BiStrategy::TranslationOnly);
        assert_eq!(md, "| a\\|b | x\\|y |\n| --- | --- |\n| d | e |");
        // 对照形态：译文中竖线同样转义
        let md_inline = to_bilingual_markdown(&doc, &translated);
        assert!(md_inline.contains("c<br>x\\|y"), "{md_inline}");
    }

    /// 往返闭环：提取侧发出的全部 id（译文 = 单元文本加前缀）喂入仅译文
    /// 回写——段落与表格格全替换，Heading 保持层级，网格结构不变。
    #[test]
    fn roundtrip_translation_only_full_feed() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    heading("数据字典"),
                    para("字段说明如下。"),
                    table(&[&["字段", "类型"], &["id", "整数"]], 1),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 2 + 4, "标题 + 段落 + 2×2 格");
        let translated: Vec<TranslationPair> = units
            .iter()
            .map(|u| (u.id.clone(), format!("译-{}", u.text)).into())
            .collect();
        let md = to_bilingual_markdown_with(&doc, &translated, BiStrategy::TranslationOnly);
        assert_eq!(
            md,
            "# 译-数据字典\n\n译-字段说明如下。\n\n| 译-字段 | 译-类型 |\n| --- | --- |\n| 译-id | 译-整数 |"
        );
        // 原表格行不复存在（精确串等价已证明全替换；此处防「原样残留」回归）
        assert!(!md.contains("| 字段 | 类型 |"), "原表格行不应残留：{md}");
        assert!(!md.lines().any(|l| l == "数据字典" || l.starts_with("字段说明如下。")));
    }

    // -------------------------------------------------------------------------
    // 双语回写
    // -------------------------------------------------------------------------

    /// 原/译同段交错：2 原块 1 译文 → 顺序 原1/译1（不空行直连）/原2。
    #[test]
    fn bilingual_inline_interleaves() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![para("第一段原文。"), para("第二段原文。")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let translated = vec![("p0-b0".to_string(), "第一段译文。".to_string()).into()];
        let md = to_bilingual_markdown(&doc, &translated);
        // 译文紧贴原文（对照紧凑），块与块之间仍空行分隔
        assert_eq!(md, "第一段原文。\n第一段译文。\n\n第二段原文。");
    }

    /// 仅译文：有译文的块文本整体替换，无译文的保留原文。
    #[test]
    fn translation_only_replaces() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![para("第一段原文。"), para("第二段原文。")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let translated = vec![("p0-b0".to_string(), "第一段译文。".to_string()).into()];
        let md = to_bilingual_markdown_with(&doc, &translated, BiStrategy::TranslationOnly);
        assert_eq!(md, "第一段译文。\n\n第二段原文。");
        assert!(!md.contains("第一段原文。"), "原文被译文替换，不再出现");
    }

    /// 原文完整 + 附录：正文与 to_markdown 一致，译文按 chapter 分组，
    /// 无章节的归（无章节）。
    #[test]
    fn original_plus_appendix_groups_by_chapter() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    para("开篇无章节。"), // b0：首 Heading 之前 → chapter None
                    heading("第一章 起"), // b1
                    para("甲段原文。"),   // b2 → 第一章
                    heading("第二章 承"), // b3
                    para("乙段原文。"),   // b4 → 第二章
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let translated = vec![
            ("p0-b0".to_string(), "开篇译文。".to_string()).into(),
            ("p0-b2".to_string(), "甲段译文。".to_string()).into(),
            ("p0-b4".to_string(), "乙段译文。".to_string()).into(),
        ];
        let md = to_bilingual_markdown_with(&doc, &translated, BiStrategy::OriginalPlusAppendix);
        let expected = format!(
            "{}\n\n---\n\n# 译文\n\n## （无章节）\n\n开篇译文。\n\n## 第一章 起\n\n甲段译文。\n\n## 第二章 承\n\n乙段译文。",
            to_markdown(&doc)
        );
        assert_eq!(md, expected);
    }

    /// 未匹配的译文 id 忽略：全部未命中时输出等于纯原文 Markdown。
    #[test]
    fn unmatched_translation_ids_ignored() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![para("第一段原文。"), para("第二段原文。")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let translated = vec![
            ("p9-b9".to_string(), "错页译文。".to_string()).into(),
            ("p0-b99".to_string(), "错块译文。".to_string()).into(),
        ];
        assert_eq!(
            to_bilingual_markdown(&doc, &translated),
            to_markdown(&doc),
            "没有 id 命中：BilingualInline 退化为纯原文"
        );
    }

    /// id 与提取侧完全一致：to_translation_units 发出的每个 id 都能被回写
    /// 命中——全部译文喂入后，BilingualInline 输出里每个单元都是
    /// 「原文渲染 + 译文」直连成对（行数 = 单元×2 + 非单元块行数）。
    #[test]
    fn id_matches_extraction_side() {
        let doc = Document {
            pages: vec![
                Page {
                    name: Some("7".into()),
                    blocks: vec![
                        heading("标题甲"), // b0 单元
                        para("正文一句。"), // b1 单元
                        Block {
                            kind: BlockKind::Code,
                            content: "let x = 1;".into(),
                            ..Default::default()
                        }, // b2 非翻译块，原样渲染
                        para("乙"),        // b3：1 字符 < min_chars，提取侧不发锚点
                        list(false, "甲项"), // b4 单元
                        Block {
                            kind: BlockKind::Quote,
                            content: "引文一句。".into(),
                            ..Default::default()
                        }, // b5 单元
                    ],
                    ..Default::default()
                },
                Page {
                    // 无名页 → 页序 "1"（与提取侧同口径）
                    blocks: vec![para("跨页一段。")],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let units = to_translation_units(&doc);
        assert_eq!(
            units.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            ["p7-b0", "p7-b1", "p7-b4", "p7-b5", "p1-b0"]
        );
        // 用提取侧发出的全部 id（译文取单元文本加前缀，逐单元可分辨）喂入回写
        let translated: Vec<TranslationPair> = units
            .iter()
            .map(|u| (u.id.clone(), format!("译-{}", u.text)).into())
            .collect();
        let md = to_bilingual_markdown(&doc, &translated); // BilingualInline
        // 全命中：每个单元的原文渲染与译文直连成对出现——漏命中即退化为
        // 原文单独成段，与期望串不符；Heading 译文保持同级标题
        assert_eq!(
            md,
            "# 标题甲\n# 译-标题甲\n\n正文一句。\n译-正文一句。\n\n```\nlet x = 1;\n```\n\n乙\n\n- 甲项\n译-甲项\n\n> 引文一句。\n译-引文一句。\n\n跨页一段。\n译-跨页一段。"
        );
        // 「块数 = 原×2」口径（按非空内容行计）：每个命中单元贡献 2 行
        //（原/译各 1），非单元块贡献原行数（Code 3 行 + 被 min_chars 滤掉的
        // 短块乙 1 行）：14 = 5×2 + 4
        assert_eq!(md.lines().filter(|l| !l.is_empty()).count(), 2 * units.len() + 4);
    }

    /// md 忽略 source（md 无重解析，锚点与 doc 同对象）：三元组形态带
    /// 「错误源文本」不改变写入行为——回写依据与输入 doc 是同一份块序，
    /// ① 提取与 ④ 回写之间不存在文件变更边界，源校验无从谈起（与
    /// writeback 族「源不一致即跳过」的口径刻意相反，见金测注释）。
    #[test]
    fn bilingual_markdown_ignores_pair_source() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![para("第一段原文。")],
                ..Default::default()
            }],
            ..Default::default()
        };
        // (id, 源文本, 译文) 三元组：源文本故意与块文本不一致
        let pairs = vec![(
            "p0-b0".to_string(),
            "完全无关的别处文本".to_string(),
            "第一段译文。".to_string(),
        )
            .into()];
        assert_eq!(to_bilingual_markdown(&doc, &pairs), "第一段原文。\n第一段译文。");
        assert_eq!(
            to_bilingual_markdown_with(&doc, &pairs, BiStrategy::TranslationOnly),
            "第一段译文。"
        );
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
