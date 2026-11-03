// FB2 后端：结构映射设计参考 document-parser 0.1.0-alpha.18 的 fb2 loader
// （MIT OR Apache-2.0，crates.io 存档 vendor/，见 THIRD_PARTY_NOTICES.md）重新实现——
// B 侧用 roxmltree 建 DOM、产出 HTML 再喂 HTML 扫描器；docforge 按 item 模型
// （level 标题 / paragraph / table）用 quick-xml 流式直提（docparse-plan §7 M5 行）。
//! FB2（FictionBook 2.x）电子书后端（feature = "fb2"，翻译场景的俄语书库主力格式）。
//!
//! FB2 是**单个 XML 文件**（不像 EPUB 是 ZIP 容器；`.fb2.zip` 打包件会解包取内层
//! 首个 `.fb2` 条目，B 语义），结构：`<FictionBook>` → `<description><title-info>`
//!（书名/作者/语言/封面）、若干 `<body>`（无 `name` 属性的第一份为主文；带 `name`
//!（如 `notes`）的附加 body 是脚注/注释）、`<binary>`（base64 内嵌图片）。
//!
//! 提取口径（对齐 [`crate::backends::epub`] 的分章语义）：
//!   * 主文按顶层 `<section>` 分页（= 章），页名取该 section `<title>`；嵌套
//!     section 的 `<title>` 按嵌套深度映射 `RawTextItem.level`（1–6），
//!     `<subtitle>` 比所在 section 深一级；
//!   * `<p>` 产 paragraph item（内联 emphasis/strong 等标记拍平，`<empty-line/>`
//!     折行）；`<poem>` 每个 `<stanza>` 成一段（`<v>` 行间 `\n`）；`<table>` 产
//!     结构化 `TableData`（B 的 loader 映射齐全，但对齐 docforge item 模型）；
//!   * 脚注 body 每逢 `<section>` 产一条 `meta["region"]="footnote"` 的 item
//!     （`assemble` 还原成 `BlockKind::Note` 且不进页文本——docx 脚注同口径），
//!     整体聚成一个尾页，页名取 body 的 `name` 属性；
//!   * `<binary>` 仅在 [`Fb2RawBackend::ocr_images`] 打开时 base64 解码进首页
//!     `images`（opt-in，对齐 EPUB 五件套的图片提取）；正文内联 `<image>` 引用
//!     不产块（id 可经 `fb2.cover_id` / binary 自身追溯）。
//!
//! 编码：UTF-8 直读；非 UTF-8 时嗅探 XML 声明的 `encoding=`（前 1KB），feature
//! `encoding` 下用 encoding_rs 解码，未开则 lossy 兜底（对齐 epub 的 `decode_entry`）。

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::error::{DocError, Result};
use crate::model::{Block, BlockKind, Document, Image, MetaValue, Page, TableData};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawImage, RawPage, RawTextItem};
use crate::registry::Registry;

/// section 递归上限：只防病态嵌套的栈溢出（标题层级本身封顶 6）。
const MAX_SECTION_DEPTH: u8 = 32;
/// epigraph/cite 容器递归上限（同上，正常文档不会超过 2–3 层）。
const MAX_FLOW_DEPTH: u8 = 8;

// ── XML 小工具（与 epub.rs 同一套 quick-xml 0.42 适配） ───────────────

fn attrs(e: &BytesStart<'_>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for a in e.attributes().flatten() {
        let key = a.key.local_name().as_ref().to_string();
        let val = quick_xml::escape::unescape(&a.value)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| a.value.to_string());
        out.insert(key, val);
    }
    out
}

/// quick-xml 0.42：`&amp;` / `&#65;` 是独立的 `Event::GeneralRef`，按 0.37 语义就地解码。
fn push_ref(buf: &mut String, e: &quick_xml::events::BytesRef<'_>) {
    match e.resolve_char_ref() {
        Ok(Some(ch)) => buf.push(ch),
        _ => {
            let entity = format!("&{};", e.xml10_content());
            if let Ok(t) = quick_xml::escape::unescape(&entity) {
                buf.push_str(&t);
            }
        }
    }
}

/// Text 事件取内容并解实体（0.38+ 不再自动 unescape）。
fn text_of(e: &quick_xml::events::BytesText<'_>) -> Option<String> {
    let raw = e.xml10_content();
    quick_xml::escape::unescape(&raw)
        .ok()
        .map(|v| v.into_owned())
}

fn xml_err(what: &str, e: impl std::fmt::Display) -> DocError {
    DocError::corrupt(format!("FB2 {what} 解析失败: {e}"))
}

/// 跳过当前子树（调用前已消费其 Start；吞到匹配的 End 为止，畸形截断不报错）。
fn skip_subtree(reader: &mut Reader<&[u8]>) {
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

/// 收集纯文本直到 `</end>`（内层标记透明；书名/语言这类叶子元素用）。
fn plain_text_until(reader: &mut Reader<&[u8]>, end: &str) -> Result<String> {
    let mut out = String::new();
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(ref e)) => {
                if e.name().local_name().as_ref() == end {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                } else {
                    depth = depth.saturating_sub(1);
                }
            }
            Ok(Event::Text(ref t)) => {
                if let Some(v) = text_of(t) {
                    out.push_str(&v);
                }
            }
            Ok(Event::GeneralRef(ref e)) => push_ref(&mut out, e),
            Ok(Event::CData(ref c)) => out.push_str(c),
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err(end, e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// 段落内联文本直到 `</end>`：emphasis/strong/sub/sup/code/a 透明拍平，
/// `<empty-line/>` 折行；内联 `<image/>` 引用不产文本（图片走 binary 提取）。
fn inline_until(reader: &mut Reader<&[u8]>, end: &str) -> Result<String> {
    let mut out = String::new();
    let mut buf = Vec::new();
    let mut depth = 1usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().local_name().as_ref() == "empty-line" {
                    out.push('\n');
                }
                depth += 1;
            }
            Ok(Event::Empty(ref e)) => {
                if e.name().local_name().as_ref() == "empty-line" {
                    out.push('\n');
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().local_name().as_ref() == end {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                } else {
                    depth = depth.saturating_sub(1);
                }
            }
            Ok(Event::Text(ref t)) => {
                if let Some(v) = text_of(t) {
                    out.push_str(&v);
                }
            }
            Ok(Event::GeneralRef(ref e)) => push_ref(&mut out, e),
            Ok(Event::CData(ref c)) => out.push_str(c),
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err(end, e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// `<title>` 文本（title 由 `<p>` 组成，B 语义：各段文本以空格连接）。
fn title_text(reader: &mut Reader<&[u8]>) -> Result<String> {
    let mut out = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::End(ref e)) => match e.name().local_name().as_ref() {
                "title" => break,
                "p" => out.push(' '),
                _ => {}
            },
            Ok(Event::Text(ref t)) => {
                if let Some(v) = text_of(t) {
                    out.push_str(&v);
                }
            }
            Ok(Event::GeneralRef(ref e)) => push_ref(&mut out, e),
            Ok(Event::CData(ref c)) => out.push_str(c),
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("title", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(out.split_whitespace().collect::<Vec<_>>().join(" "))
}

// ── item 装配小件 ────────────────────────────────────────────────────

/// 非空文本才成 item；level 就地封顶 1–6。
fn push_item(items: &mut Vec<RawTextItem>, text: String, level: Option<u8>) {
    let t = text.trim();
    if t.is_empty() {
        return;
    }
    items.push(RawTextItem {
        text: t.to_string(),
        level: level.map(|l| l.clamp(1, 6)),
        ..Default::default()
    });
}

/// 表格 → 单个 table item（`text` 同步展开为「行内 `,` 连接、行间 `\n`」，
/// 与 office 后端 docx 的 table_item 同口径）。
fn push_table(items: &mut Vec<RawTextItem>, rows: Vec<Vec<String>>) {
    if rows.is_empty() {
        return;
    }
    let text = rows
        .iter()
        .map(|r| r.join(","))
        .collect::<Vec<_>>()
        .join("\n");
    items.push(RawTextItem {
        text,
        table: Some(TableData {
            rows,
            ..Default::default()
        }),
        ..Default::default()
    });
}

/// 页名：该页 items 里第一条一级标题（body 卷首 title / 顶层 section 的 title）。
fn page_title(items: &[RawTextItem]) -> Option<String> {
    items
        .iter()
        .find(|i| i.level == Some(1) && !i.text.trim().is_empty())
        .map(|i| i.text.trim().to_string())
}

// ── 正文 / 脚注 body ─────────────────────────────────────────────────

/// 主文 body → 页列表（顶层 section 一页；section 前的卷首内容单独一页）。
fn parse_main_body(reader: &mut Reader<&[u8]>) -> Result<Vec<RawPage>> {
    let mut preamble: Vec<RawTextItem> = Vec::new();
    let mut sections: Vec<Vec<RawTextItem>> = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "title" => push_item(&mut preamble, title_text(reader)?, Some(1)),
                    "section" => {
                        let mut items = Vec::new();
                        parse_section(reader, 1, &mut items)?;
                        sections.push(items);
                    }
                    "p" => push_item(&mut preamble, inline_until(reader, "p")?, None),
                    "poem" => poem_until(reader, &mut preamble)?,
                    "table" => push_table(&mut preamble, table_rows(reader)?),
                    "epigraph" | "cite" => flow_until(reader, n.as_ref(), 1, &mut preamble)?,
                    // 内联图片引用不产块（binary 提取走 ocr_images）
                    "image" => {}
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "body" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("body", e)),
            _ => {}
        }
        buf.clear();
    }
    let mut pages = Vec::new();
    if !preamble.is_empty() {
        pages.push(RawPage {
            name: page_title(&preamble),
            items: preamble,
            ..Default::default()
        });
    }
    for items in sections {
        pages.push(RawPage {
            name: page_title(&items),
            items,
            ..Default::default()
        });
    }
    Ok(pages)
}

/// `<section>`（可嵌套）：`<title>` 按嵌套深度映射 level，嵌套 section 并入当前页。
fn parse_section(
    reader: &mut Reader<&[u8]>,
    depth: u8,
    items: &mut Vec<RawTextItem>,
) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "title" => push_item(items, title_text(reader)?, Some(depth)),
                    // 小标题：比所在 section 深一级（B 映射 title→h2 / subtitle→h3）
                    "subtitle" => {
                        let lv = depth.saturating_add(1).min(6);
                        push_item(items, inline_until(reader, "subtitle")?, Some(lv));
                    }
                    "section" => {
                        if depth < MAX_SECTION_DEPTH {
                            parse_section(reader, depth.saturating_add(1), items)?;
                        } else {
                            skip_subtree(reader); // 病态嵌套：丢弃而非栈溢出
                        }
                    }
                    "p" => push_item(items, inline_until(reader, "p")?, None),
                    "poem" => poem_until(reader, items)?,
                    "table" => push_table(items, table_rows(reader)?),
                    "epigraph" | "cite" => flow_until(reader, n.as_ref(), 1, items)?,
                    "image" => {}
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "section" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("section", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// epigraph / cite 容器：内部按流式元素展开（p / poem / table / 嵌套引用）。
fn flow_until(
    reader: &mut Reader<&[u8]>,
    end: &str,
    depth: u8,
    items: &mut Vec<RawTextItem>,
) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "p" => push_item(items, inline_until(reader, "p")?, None),
                    "subtitle" => push_item(items, inline_until(reader, "subtitle")?, None),
                    "poem" => poem_until(reader, items)?,
                    "table" => push_table(items, table_rows(reader)?),
                    "text-author" => push_item(items, inline_until(reader, "text-author")?, None),
                    "epigraph" | "cite" => {
                        if depth < MAX_FLOW_DEPTH {
                            flow_until(reader, n.as_ref(), depth + 1, items)?;
                        } else {
                            skip_subtree(reader);
                        }
                    }
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == end => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err(end, e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// `<poem>`：每个 `<stanza>` 成一段（`<v>` 行间 `\n`，B 的 verse 语义），
/// `<text-author>` 单独成段，诗题按普通段落处理。
fn poem_until(reader: &mut Reader<&[u8]>, items: &mut Vec<RawTextItem>) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "title" => push_item(items, title_text(reader)?, None),
                    "stanza" => {
                        let lines = stanza_lines(reader)?;
                        if !lines.is_empty() {
                            push_item(items, lines.join("\n"), None);
                        }
                    }
                    "text-author" => push_item(items, inline_until(reader, "text-author")?, None),
                    "epigraph" | "cite" => flow_until(reader, n.as_ref(), 1, items)?,
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "poem" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("poem", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// `<stanza>` 的 `<v>` 行（已 trim；空行丢弃）。
fn stanza_lines(reader: &mut Reader<&[u8]>) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "v" => {
                        let t = inline_until(reader, "v")?;
                        if !t.trim().is_empty() {
                            lines.push(t.trim().to_string());
                        }
                    }
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "stanza" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("stanza", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(lines)
}

/// `<table>` → rows × cells（`td` / `th` 等价；单元格内联标记拍平）。
fn table_rows(reader: &mut Reader<&[u8]>) -> Result<Vec<Vec<String>>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "tr" => rows.push(row_cells(reader)?),
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "table" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("table", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(rows)
}

fn row_cells(reader: &mut Reader<&[u8]>) -> Result<Vec<String>> {
    let mut cells = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "td" | "th" => {
                        cells.push(inline_until(reader, n.as_ref())?.trim().to_string())
                    }
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(ref e)) => {
                // 自闭合空单元格保留占位（列不错位）
                let n = e.name().local_name();
                if n.as_ref() == "td" || n.as_ref() == "th" {
                    cells.push(String::new());
                }
            }
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "tr" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("tr", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(cells)
}

/// 脚注 body（带 `name` 的附加 body）：每个 `<section>` 一条脚注，
/// text = 标题 + 各段（`\n\n` 连接），`meta["region"]="footnote"`（id 顺手带上）。
fn parse_notes_body(reader: &mut Reader<&[u8]>) -> Result<Vec<RawTextItem>> {
    let mut items = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "section" => {
                        let id = attrs(e)
                            .get("id")
                            .cloned()
                            .filter(|s| !s.is_empty());
                        let (title, texts) = note_section(reader)?;
                        let mut parts: Vec<String> = Vec::new();
                        if !title.is_empty() {
                            parts.push(title);
                        }
                        parts.extend(texts.into_iter().filter(|t| !t.is_empty()));
                        if parts.is_empty() {
                            continue;
                        }
                        let mut meta = BTreeMap::new();
                        meta.insert("region".to_string(), MetaValue::from("footnote"));
                        if let Some(id) = id {
                            meta.insert("id".to_string(), MetaValue::from(id));
                        }
                        items.push(RawTextItem {
                            text: parts.join("\n\n"),
                            meta,
                            ..Default::default()
                        });
                    }
                    // 裸段落（无 section 包裹的畸形脚注 body）：同样按脚注收
                    "p" => {
                        let text = inline_until(reader, "p")?;
                        if !text.trim().is_empty() {
                            let mut meta = BTreeMap::new();
                            meta.insert("region".to_string(), MetaValue::from("footnote"));
                            items.push(RawTextItem {
                                text: text.trim().to_string(),
                                meta,
                                ..Default::default()
                            });
                        }
                    }
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "body" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("notes body", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(items)
}

/// 脚注 section 的（标题, 各段文本）。
fn note_section(reader: &mut Reader<&[u8]>) -> Result<(String, Vec<String>)> {
    let mut title = String::new();
    let mut texts = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "title" => title = title_text(reader)?,
                    "p" | "subtitle" => {
                        texts.push(inline_until(reader, n.as_ref())?.trim().to_string())
                    }
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "section" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("note section", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok((title, texts))
}

// ── description / title-info 元数据 ──────────────────────────────────

/// `description/title-info` 提取结果（取不到的字段为空）。
#[derive(Debug, Clone, Default)]
struct TitleInfo {
    title: String,
    authors: Vec<String>,
    language: String,
    /// 封面图（`coverpage/image @l:href` 指向的 `<binary>` id，已去 `#`）。
    cover_id: Option<String>,
}

/// 只认 `title-info`（`src-title-info` 的书名/语言是译源信息，不采纳）。
fn parse_description(reader: &mut Reader<&[u8]>) -> Result<TitleInfo> {
    let mut out = TitleInfo::default();
    let mut in_title_info = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "title-info" => in_title_info = true,
                    "book-title" if in_title_info && out.title.is_empty() => {
                        out.title = plain_text_until(reader, "book-title")?
                            .trim()
                            .to_string();
                    }
                    "author" if in_title_info => {
                        let a = author_text(reader)?;
                        if !a.is_empty() {
                            out.authors.push(a);
                        }
                    }
                    "lang" if in_title_info && out.language.is_empty() => {
                        out.language =
                            plain_text_until(reader, "lang")?.trim().to_string();
                    }
                    "coverpage" if in_title_info && out.cover_id.is_none() => {
                        out.cover_id = coverpage_id(reader)?;
                    }
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::End(ref e)) => match e.name().local_name().as_ref() {
                "title-info" => in_title_info = false,
                "description" => break,
                _ => {}
            },
            Ok(Event::Eof) => break, // 畸形截断：已收到的元数据照常返回
            Err(e) => return Err(xml_err("description", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// `<author>`：first-name + last-name 拼全名，缺则 nickname（B 语义）。
fn author_text(reader: &mut Reader<&[u8]>) -> Result<String> {
    let mut first = String::new();
    let mut last = String::new();
    let mut nick = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "first-name" => first = plain_text_until(reader, "first-name")?,
                    "last-name" => last = plain_text_until(reader, "last-name")?,
                    "nickname" => nick = plain_text_until(reader, "nickname")?,
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "author" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("author", e)),
            _ => {}
        }
        buf.clear();
    }
    let full = [first.trim(), last.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if full.is_empty() {
        Ok(nick.trim().to_string())
    } else {
        Ok(full)
    }
}

/// `<coverpage>` 里首个 `<image l:href="#id">` 的 id。
fn coverpage_id(reader: &mut Reader<&[u8]>) -> Result<Option<String>> {
    let mut out = None;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().local_name().as_ref() == "image" {
                    if let Some(href) = attrs(e).get("href") {
                        out = Some(href.trim_start_matches('#').to_string());
                    }
                }
            }
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "coverpage" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("coverpage", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(out.filter(|s| !s.is_empty()))
}

// ── binary（base64 图片）与文件读取 ─────────────────────────────────

/// `<binary>`：content-type + base64 解码字节（id 缺失 / base64 非法 → 跳过，B 语义；
/// id 本身不透传——RawImage 无该字段，内联引用由 meta `fb2.cover_id` 追溯）。
struct Binary {
    content_type: String,
    data: Vec<u8>,
}

fn read_binary(reader: &mut Reader<&[u8]>, e: &BytesStart<'_>) -> Result<Option<Binary>> {
    let a = attrs(e);
    let id = a.get("id").cloned().unwrap_or_default();
    let content_type = a.get("content-type").cloned().unwrap_or_default();
    let mut encoded = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(ref t)) => {
                if let Some(v) = text_of(t) {
                    encoded.push_str(&v);
                }
            }
            Ok(Event::GeneralRef(ref r)) => push_ref(&mut encoded, r),
            Ok(Event::CData(ref c)) => encoded.push_str(c),
            Ok(Event::End(ref end)) if end.name().local_name().as_ref() == "binary" => break,
            Ok(Event::Eof) => break,
            Err(err) => return Err(xml_err("binary", err)),
            _ => {}
        }
        buf.clear();
    }
    // base64 允许换行/空白填充，先全部滤掉再解码
    let encoded: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    if id.is_empty() || encoded.is_empty() {
        return Ok(None);
    }
    match BASE64.decode(encoded.as_bytes()) {
        Ok(data) => Ok(Some(Binary { content_type, data })),
        Err(_) => Ok(None),
    }
}

/// content-type → 扩展名（`image/jpeg` → `jpg`；非 image/* 为空串）。
fn ext_of_content_type(ct: &str) -> String {
    let sub = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match sub.as_str() {
        "image/jpeg" | "image/jpg" => "jpg".to_string(),
        "image/png" => "png".to_string(),
        "image/gif" => "gif".to_string(),
        other => other.strip_prefix("image/").unwrap_or("").to_string(),
    }
}

/// 读文件：`.fb2.zip` 打包件（ZIP magic 判定）解包取内层首个 `.fb2` 条目（B 语义）。
fn read_fb2_bytes(path: &str) -> Result<Vec<u8>> {
    let bytes =
        std::fs::read(path).map_err(|e| DocError::corrupt(format!("FB2 打不开: {e}")))?;
    if bytes.starts_with(b"PK\x03\x04") {
        fb2_from_zip(bytes)
    } else {
        Ok(bytes)
    }
}

fn fb2_from_zip(bytes: Vec<u8>) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes[..]))
        .map_err(|e| DocError::corrupt(format!("FB2 ZIP 打不开: {e}")))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| DocError::corrupt(format!("FB2 ZIP 条目读取失败: {e}")))?;
        let name = entry
            .name()
            .map_err(|e| DocError::corrupt(format!("FB2 ZIP 条目名读取失败: {e}")))?;
        if !name.to_ascii_lowercase().ends_with(".fb2") {
            continue;
        }
        let mut out = Vec::new();
        entry
            .read_to_end(&mut out)
            .map_err(|e| DocError::corrupt(format!("FB2 ZIP 解压失败: {e}")))?;
        return Ok(out);
    }
    Err(DocError::corrupt("FB2 ZIP 内未找到 .fb2 条目"))
}

/// 整体解码：UTF-8（绝大多数）→ XML 声明嗅探的非 UTF-8（feature `encoding`
/// 下用 encoding_rs；未开则 lossy）→ lossy 兜底（对齐 epub 的 `decode_entry`）。
fn decode_fb2(bytes: &[u8]) -> String {
    if std::str::from_utf8(bytes).is_ok() {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    // XML 声明嗅探：前 1KB 内找 encoding="..."
    let head = &bytes[..bytes.len().min(1024)];
    let head_lossy = String::from_utf8_lossy(head);
    if let Some(enc) = head_lossy
        .split("encoding")
        .nth(1)
        .and_then(|rest| {
            rest.trim_start()
                .strip_prefix('=')
                .map(|r| r.trim_start().trim_start_matches('"').trim_start_matches('\''))
        })
        .and_then(|r| r.split(['"', '\'', ' ', '?']).next())
        .map(str::to_ascii_lowercase)
    {
        #[cfg(feature = "encoding")]
        {
            for label in [&enc, &enc.replace('_', "-")] {
                if let Some(cs) = encoding_rs::Encoding::for_label(label.as_bytes()) {
                    if cs == encoding_rs::UTF_8 {
                        break; // 声明 UTF-8 但字节非法：走 lossy
                    }
                    let (s, _, had_err) = cs.decode(bytes);
                    if !had_err {
                        return s.into_owned();
                    }
                }
            }
        }
        let _ = enc; // 未开 encoding feature：嗅探结果仅供参考，lossy 兜底
    }
    String::from_utf8_lossy(bytes).into_owned()
}

// ── 单遍主解析 ──────────────────────────────────────────────────────

/// 整份 FB2 XML → RawDoc（`Fb2RawBackend::parse` 的主体）。
fn parse_fb2(xml: &str, ocr_images: bool) -> Result<RawDoc> {
    let mut reader = Reader::from_str(xml);
    // 不 trim：内联标记两侧的原始空格要保真（英文书 <em> 边界）；
    // 块级循环本来就忽略游离 Text，块内空白由 push_item / title_text 收敛。
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut title_info = TitleInfo::default();
    let mut pages: Vec<RawPage> = Vec::new();
    let mut notes: Option<RawPage> = None;
    let mut binaries: Vec<Binary> = Vec::new();
    let mut seen_root = false;
    let mut seen_main = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                let name = n.as_ref().to_string();
                if !seen_root {
                    seen_root = true;
                    if name != "FictionBook" {
                        return Err(DocError::corrupt(format!(
                            "不是 FB2（根元素是 {name}，应为 FictionBook）"
                        )));
                    }
                    buf.clear();
                    continue;
                }
                match name.as_str() {
                    "description" => title_info = parse_description(&mut reader)?,
                    "body" => {
                        let named = attrs(e)
                            .get("name")
                            .cloned()
                            .filter(|s| !s.is_empty());
                        match named {
                            // 第一份无 name 的 body 是主文；第二份视为畸形，跳过
                            None if !seen_main => {
                                seen_main = true;
                                pages = parse_main_body(&mut reader)?;
                            }
                            Some(named) => {
                                let items = parse_notes_body(&mut reader)?;
                                match &mut notes {
                                    Some(np) => np.items.extend(items),
                                    None => {
                                        notes = Some(RawPage {
                                            name: Some(named),
                                            items,
                                            ..Default::default()
                                        })
                                    }
                                }
                            }
                            None => skip_subtree(&mut reader),
                        }
                    }
                    "binary" if ocr_images => {
                        if let Some(b) = read_binary(&mut reader, e)? {
                            binaries.push(b);
                        }
                    }
                    _ => skip_subtree(&mut reader),
                }
            }
            Ok(Event::Empty(_)) => {} // <binary/>（无数据）/ <image/>：跳过
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("XML", e)),
            _ => {}
        }
        buf.clear();
    }

    if !seen_root {
        return Err(DocError::corrupt("不是 FB2（未找到根元素 FictionBook）"));
    }
    if pages.is_empty() {
        return Err(DocError::corrupt("FB2 缺正文 body（无 section/段落）"));
    }
    if let Some(np) = notes {
        if !np.items.is_empty() {
            pages.push(np);
        }
    }

    // 图片字节提取（opt-in）：文档级 binary 统一放首页 images（FB2 无物理页，
    // 与 EPUB 五件套「manifest 图片进首页」同口径），ocr-images 处理能力消费。
    if ocr_images && !binaries.is_empty() {
        let images: Vec<RawImage> = binaries
            .into_iter()
            .map(|b| RawImage {
                data: b.data,
                ext: ext_of_content_type(&b.content_type),
                ..Default::default()
            })
            .collect();
        if let Some(first) = pages.first_mut() {
            first.images = images;
        }
    }

    let mut meta: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if !title_info.title.is_empty() {
        meta.insert("fb2.title".to_string(), title_info.title);
    }
    if !title_info.authors.is_empty() {
        meta.insert("fb2.authors".to_string(), title_info.authors.join("; "));
    }
    if !title_info.language.is_empty() {
        meta.insert("fb2.language".to_string(), title_info.language);
    }
    if let Some(c) = title_info.cover_id {
        meta.insert("fb2.cover_id".to_string(), c);
    }

    Ok(RawDoc {
        pages,
        meta,
        needs_ocr: false,
    })
}

// ── 能力 / 插件 ─────────────────────────────────────────────────────

/// FB2 原始读取后端：单 XML → items + 元数据（`ocr_images` 默认关）。
#[derive(Default)]
pub struct Fb2RawBackend {
    /// 提取 `<binary>` base64 图片进首页 `images`（opt-in，对齐 EPUB 的
    /// `ocr_images`；`ocr-images` 处理能力识别后回填 `ocr_text`）。
    pub ocr_images: bool,
}

impl ParseCapability for Fb2RawBackend {
    fn id(&self) -> &str {
        "fb2.read"
    }
    fn formats(&self) -> &[&str] {
        &["fb2"]
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let bytes = read_fb2_bytes(path)?;
        let xml = decode_fb2(&bytes);
        parse_fb2(&xml, self.ocr_images)
    }
}

/// FB2 插件：level → Heading，table → Table，`meta["region"]` → Note（不进页文本），
/// 其余 Paragraph（组装口径同 epub：块文本以空行连接成页文本）。
pub struct Fb2Plugin;

impl FormatPlugin for Fb2Plugin {
    fn id(&self) -> &str {
        "fb2"
    }
    fn formats(&self) -> &[&str] {
        &["fb2"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["fb2.read"]
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        // opt-in 链：能力未注册时 resolve 静默跳过（registry filter_map 语义）
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
                let mut blocks: Vec<Block> = Vec::new();
                let mut parts: Vec<String> = Vec::new();
                for it in &rp.items {
                    if let Some(t) = &it.table {
                        blocks.push(Block {
                            kind: BlockKind::Table,
                            content: t.rows.first().map(|r| r.join(" | ")).unwrap_or_default(),
                            table: Some(t.clone()),
                            ..Default::default()
                        });
                        if !it.text.is_empty() {
                            parts.push(it.text.clone());
                        }
                        continue;
                    }
                    let text = it.text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    // 脚注 → Note 块且不进页文本（docx 脚注同口径）
                    let is_region = matches!(it.meta.get("region"), Some(MetaValue::Str(s)) if !s.is_empty());
                    let mut block = if is_region {
                        Block {
                            kind: BlockKind::Note,
                            content: text.to_string(),
                            ..Default::default()
                        }
                    } else if let Some(lv) = it.level {
                        Block {
                            kind: BlockKind::Heading,
                            content: text.to_string(),
                            level: Some(lv.clamp(1, 6)),
                            ..Default::default()
                        }
                    } else {
                        Block {
                            kind: BlockKind::Paragraph,
                            content: text.to_string(),
                            ..Default::default()
                        }
                    };
                    block.meta = it.meta.clone();
                    if !is_region {
                        parts.push(text.to_string());
                    }
                    blocks.push(block);
                }
                // 首页 images（ocr_images 提取 + ocr-images 处理链回填）→ Page.images
                let images: Vec<Image> = rp
                    .images
                    .iter()
                    .map(|im| Image {
                        data: im.data.clone(),
                        bbox: im.bbox,
                        dedup_key: None,
                        ocr_text: im.ocr_text.clone(),
                    })
                    .collect();
                Page {
                    name: rp.name.clone(),
                    blocks,
                    text: parts.join("\n\n"),
                    images,
                    ..Default::default()
                }
            })
            .collect();
        let mut doc = Document {
            pages,
            ..Default::default()
        };
        for key in ["fb2.title", "fb2.authors", "fb2.language", "fb2.cover_id"] {
            if let Some(v) = raw.meta.get(key) {
                doc.meta.extra.insert(key.to_string(), v.clone());
            }
        }
        doc
    }
}

/// 注册 FB2 后端与插件（默认不开图片提取）。
pub fn register_defaults(reg: &mut Registry) {
    let _ = reg.register_parse(Arc::new(Fb2RawBackend::default()), None);
    reg.register_plugin(Arc::new(Fb2Plugin));
}

/// 同 [`register_defaults`]，但后端带 `ocr_images` 开关（提取 `<binary>` 图片字节）。
pub fn register_with_ocr_images(reg: &mut Registry, ocr_images: bool) {
    let _ = reg.register_parse(Arc::new(Fb2RawBackend { ocr_images }), None);
    reg.register_plugin(Arc::new(Fb2Plugin));
}

// ── 测试 ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 8 字节 PNG 头（epub 五件套测试同款）：只验提取与解码，不需要完整图。
    const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    fn book_xml() -> String {
        let cover_b64 = BASE64.encode(PNG_MAGIC);
        format!(
            r##"<?xml version="1.0" encoding="utf-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/FictionBook/2.0" xmlns:l="http://www.w3.org/1999/xlink">
<description>
  <title-info>
    <genre>poetry</genre>
    <author><first-name>李</first-name><last-name>雷</last-name></author>
    <author><nickname>夜行者</nickname></author>
    <book-title>测试之书</book-title>
    <lang>zh</lang>
    <coverpage><image l:href="#cover.png"/></coverpage>
  </title-info>
  <src-title-info><book-title>译源书名</book-title><lang>en</lang></src-title-info>
</description>
<body>
  <title><p>测试之书</p></title>
  <epigraph><p>卷首语</p></epigraph>
  <section id="ch1">
    <title><p>第一章</p></title>
    <p>第一章正文，带<strong>强调</strong>与实体 &amp; 转义。</p>
    <section>
      <title><p>第一节</p></title>
      <p>第一节正文</p>
    </section>
    <subtitle>节尾小结</subtitle>
  </section>
  <section id="ch2">
    <title><p>第二章</p></title>
    <poem>
      <stanza><v>白日依山尽</v><v>黄河入海流</v></stanza>
      <stanza><v>欲穷千里目</v><v>更上一层楼</v></stanza>
      <text-author>—— 王之涣</text-author>
    </poem>
    <table><tr><td>甲</td><td>乙</td></tr><tr><td>1</td><td>2</td></tr></table>
  </section>
</body>
<body name="notes">
  <section id="n1"><title><p>1</p></title><p>第一条脚注</p></section>
  <section id="n2"><title><p>2</p></title><p>第二条脚注</p></section>
</body>
<binary id="cover.png" content-type="image/png">{cover_b64}</binary>
</FictionBook>"##
        )
    }

    fn write_file(dir_name: &str, file_name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("fb2-{dir_name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn parse_str(dir_name: &str, ocr_images: bool) -> RawDoc {
        let path = write_file(dir_name, "book.fb2", book_xml().as_bytes());
        let raw = Fb2RawBackend { ocr_images }
            .parse(path.to_str().unwrap())
            .unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        raw
    }

    #[test]
    fn reads_metadata_chapters_footnotes_and_images() {
        let raw = parse_str("full", true);

        // 元数据：title-info 三件 + 封面 id；src-title-info 不采纳
        let get = |k: &str| raw.meta.get(k).map(|s| s.as_str());
        assert_eq!(get("fb2.title"), Some("测试之书"));
        assert_eq!(get("fb2.authors"), Some("李 雷; 夜行者"));
        assert_eq!(get("fb2.language"), Some("zh"));
        assert_eq!(get("fb2.cover_id"), Some("cover.png"));

        // 分页：卷首（body 级 title+epigraph）→ 第一章 → 第二章 → 脚注尾页
        let names: Vec<Option<&str>> = raw.pages.iter().map(|p| p.name.as_deref()).collect();
        assert_eq!(
            names,
            vec![Some("测试之书"), Some("第一章"), Some("第二章"), Some("notes")]
        );

        // 章节层级：一级 section 标题 level 1，嵌套 section / subtitle level 2
        let ch1 = &raw.pages[1].items;
        assert_eq!(ch1[0].text, "第一章");
        assert_eq!(ch1[0].level, Some(1));
        assert_eq!(ch1[1].text, "第一章正文，带强调与实体 & 转义。");
        assert_eq!(ch1[1].level, None);
        assert_eq!(ch1[2].text, "第一节");
        assert_eq!(ch1[2].level, Some(2));
        assert_eq!(ch1[4].text, "节尾小结");
        assert_eq!(ch1[4].level, Some(2));

        // 诗歌：每 stanza 一段、v 行间换行；表格：TableData + 展开文本
        let ch2 = &raw.pages[2].items;
        assert_eq!(ch2[1].text, "白日依山尽\n黄河入海流");
        assert_eq!(ch2[3].text, "—— 王之涣");
        assert_eq!(ch2[4].table.as_ref().unwrap().rows, vec![vec!["甲", "乙"], vec!["1", "2"]]);
        assert_eq!(ch2[4].text, "甲,乙\n1,2");

        // 脚注：region=footnote 语义 + section id，标题并入首行
        let notes = &raw.pages[3].items;
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].text, "1\n\n第一条脚注");
        assert_eq!(
            notes[0].meta.get("region"),
            Some(&MetaValue::from("footnote"))
        );
        assert_eq!(notes[1].meta.get("id"), Some(&MetaValue::from("n2")));

        // 图片：binary base64 解码进首页 images（content-type → ext）
        assert_eq!(raw.pages[0].images.len(), 1);
        assert_eq!(raw.pages[0].images[0].ext, "png");
        assert_eq!(raw.pages[0].images[0].data, PNG_MAGIC.to_vec());
    }

    #[test]
    fn ocr_images_default_off() {
        let raw = parse_str("ocioff", false);
        assert!(raw.pages.iter().all(|p| p.images.is_empty()));
    }

    #[test]
    fn assemble_maps_levels_notes_and_meta() {
        let raw = parse_str("assemble", true);
        let doc = Fb2Plugin.assemble(&raw);

        // 卷首页：Heading(1) + Paragraph，页文本块间空行连接
        let p0 = &doc.pages[0];
        assert_eq!(p0.blocks[0].kind, BlockKind::Heading);
        assert_eq!(p0.blocks[0].level, Some(1));
        assert_eq!(p0.blocks[1].kind, BlockKind::Paragraph);
        assert_eq!(p0.text, "测试之书\n\n卷首语");

        // 一级章页：Heading → Paragraph → 二级 Heading（嵌套节）
        let p1 = &doc.pages[1];
        let kinds: Vec<BlockKind> = p1.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Heading,
                BlockKind::Paragraph,
                BlockKind::Heading,
                BlockKind::Paragraph,
                BlockKind::Heading
            ]
        );
        assert_eq!(p1.blocks[2].level, Some(2));

        // 表格块：content 取首行，TableData 保留行列结构
        let p2 = &doc.pages[2];
        assert_eq!(p2.blocks[4].kind, BlockKind::Table);
        assert_eq!(p2.blocks[4].content, "甲 | 乙");

        // 脚注页：Note 块 + meta region，不进页文本（docx 脚注同口径）
        let p3 = &doc.pages[3];
        assert!(p3.blocks.iter().all(|b| b.kind == BlockKind::Note));
        assert_eq!(p3.text, "");
        assert_eq!(
            p3.blocks[0].meta.get("region"),
            Some(&MetaValue::from("footnote"))
        );

        // 书籍元数据进 doc.meta.extra
        assert_eq!(
            doc.meta.extra.get("fb2.title").map(|s| s.as_str()),
            Some("测试之书")
        );
        assert_eq!(
            doc.meta.extra.get("fb2.cover_id").map(|s| s.as_str()),
            Some("cover.png")
        );
    }

    /// GBK 编码件：XML 声明嗅探（feature `encoding` 下正确解码，未开保持 lossy）。
    #[test]
    fn gbk_encoded_fb2_decoded_via_declaration() {
        let gbk_title: [u8; 4] = [0xD6, 0xD0, 0xCE, 0xC4]; // "中文"
        let mut bytes = br#"<?xml version="1.0" encoding="gbk"?>
<FictionBook><description><title-info><book-title>"#
            .to_vec();
        bytes.extend_from_slice(&gbk_title);
        bytes.extend_from_slice(
            br#"</book-title><lang>zh</lang></title-info></description><body><section><title><p>"#,
        );
        bytes.extend_from_slice(&gbk_title);
        bytes.extend_from_slice(br#"</p></title><p>body</p></section></body></FictionBook>"#);
        let path = write_file("gbk", "gbk.fb2", &bytes);
        let raw = Fb2RawBackend::default()
            .parse(path.to_str().unwrap())
            .unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let title = raw.meta.get("fb2.title").cloned().unwrap_or_default();
        #[cfg(feature = "encoding")]
        assert_eq!(title, "中文", "GBK 声明应正确解码：{title:?}");
        #[cfg(not(feature = "encoding"))]
        assert_ne!(title, "中文", "未开 encoding feature 应保持 lossy：{title:?}");
    }

    /// `.fb2.zip` 打包件：解包取内层首个 .fb2（B 语义）。
    #[test]
    fn fb2_zip_container_unwrapped() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("fb2-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("book.fb2.zip");
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        zw.start_file("readme.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(b"not a book").unwrap();
        zw.start_file("inner.fb2", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(book_xml().as_bytes()).unwrap();
        zw.finish().unwrap();

        let raw = Fb2RawBackend::default()
            .parse(path.to_str().unwrap())
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            raw.meta.get("fb2.title").map(|s| s.as_str()),
            Some("测试之书")
        );
        assert_eq!(raw.pages.len(), 4, "内层 .fb2 完整解析（含脚注尾页）");
    }

    #[test]
    fn non_fb2_input_is_corrupt() {
        // 纯文本（无根元素）
        let p1 = write_file("plain", "plain.fb2", b"just some text");
        let err = Fb2RawBackend::default()
            .parse(p1.to_str().unwrap())
            .unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        let _ = std::fs::remove_dir_all(p1.parent().unwrap());

        // XML 但根元素不是 FictionBook
        let p2 = write_file("wrongroot", "wrong.fb2", b"<html><body>x</body></html>");
        let err = Fb2RawBackend::default()
            .parse(p2.to_str().unwrap())
            .unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(err.message.contains("FictionBook"));
        let _ = std::fs::remove_dir_all(p2.parent().unwrap());

        // 空 body（无 section 也无段落）的主文
        let p3 = write_file(
            "emptybody",
            "empty.fb2",
            b"<FictionBook><body></body></FictionBook>",
        );
        let err = Fb2RawBackend::default()
            .parse(p3.to_str().unwrap())
            .unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(err.message.contains("body"));
        let _ = std::fs::remove_dir_all(p3.parent().unwrap());
    }

    /// 注册表接线：插件按 "fb2" 可寻址，解析链解析到 fb2.read。
    #[test]
    fn registry_wires_plugin_and_backend() {
        let mut reg = Registry::new();
        register_defaults(&mut reg);
        let plugin = reg.plugin_for("fb2").unwrap();
        assert_eq!(plugin.id(), "fb2");
        let pol = crate::policy::BackendPolicy::default();
        let chain = reg.resolve_parse_chain("fb2", &pol, &plugin.default_parse_chain());
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id(), "fb2.read");
    }
}
