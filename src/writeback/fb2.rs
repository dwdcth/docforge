// docforge 新增（翻译闭环①写侧）：FB2 双语回写。
//! FB2 双语回写：单 XML 重扫描对齐 + 元素级定位改写（feature = "fb2"）。
//!
//! # 对齐口径（与 `backends/fb2.rs` 提取侧逐条对应）
//!
//! * 提取侧管线：`Fb2RawBackend::default()`（主文 body：卷首 + 顶层 section
//!   分页；嵌套 section 并入当前页；脚注 body 聚成尾页）→ `Fb2Plugin::assemble`
//!   （level → Heading / table → Table / region → Note / 其余 Paragraph）。
//!   回写侧**重跑同一管线**拿权威 id/文本（`p{页名}-b{块序}`），再以同一套
//!   遍历（parse_main_body / parse_section / flow_until / poem_until /
//!   stanza_lines / title_text 的镜像）**重扫描主文 body XML**，把每个 item
//!   映回「槽位 × 字节锚点」。page 序 = 卷首?+ section*，与 raw.pages 前缀
//!   一一同位（脚注尾页在 Document 里多出一页，其 Note 块不进翻译流，
//!   回写侧直接跳过）。
//! * 定位锚点（与 FB2 内容模型对应）：
//!   - `p` / `subtitle` / `text-author`：Inline 在元素 End 后插同级 `<p>`
//!     （body/section/epigraph/cite 层级 p 合法）；TranslationOnly 替换元素
//!     内文本区（内联标记拍平，与提取侧 `inline_until` 同口径）；
//!   - `<title>`（各 `<p>` 的文本区）：Inline 在 `</title>` 后插 `<p>`；
//!     TranslationOnly 首个 title-p 换译文、其余清空（docx 首 `w:t` 同款）；
//!   - `<stanza>`（各 `<v>` 的文本区）：poem 内**不允许 `<p>`**，Inline
//!     跳过（宁漏勿错）；TranslationOnly 首 `<v>` 换译文、其余清空；
//!   - `<table>` 产 Table 块（不进翻译流）但占槽位；`<empty-line/>` /
//!     裸 `<image>` 不产 item（提取侧同口径）。
//! * 双保险：每个定位到的槽位文本与权威单元文本一致性比对，不一致跳过
//!   （宁漏勿错）；未匹配 id / 空白译文一律忽略。
//! * OriginalPlusAppendix：正文不动，主文 `</body>` 前追加
//!   `<section><title><p>译文</p></title>…</section>`（重解析自开新页）。
//!
//! # 编码与容器
//!
//! 仅支持 UTF-8 单 XML 件：非 UTF-8 拒绝（避免与 XML 声明的 encoding 失配）；
//! `.fb2.zip` 容器拒绝（解析侧会解包读内层件，回写侧不做容器重打包）。

use std::path::Path;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::backends::fb2::{Fb2Plugin, Fb2RawBackend};
use crate::emit::BiStrategy;
use crate::error::{DocError, Result};
use crate::plugin::{FormatPlugin, ParseCapability};

use super::{apply_edits, Edit, escape_xml_text, translation_map};

/// section 递归上限（与提取侧 [`crate::backends::fb2`] 同值：只防病态嵌套）。
const MAX_SECTION_DEPTH: u8 = 32;
/// epigraph/cite 容器递归上限（同上）。
const MAX_FLOW_DEPTH: u8 = 8;

/// fb2 双语回写：见模块级文档。`translated` 的 id 与
/// [`crate::emit::to_translation_units`] 同口径；未匹配 / 空白译文条目忽略；
/// 失败返回 `corrupt`。
pub fn writeback_fb2(
    in_path: impl AsRef<Path>,
    translated: &[(String, String)],
    strategy: BiStrategy,
    out: impl AsRef<Path>,
) -> Result<()> {
    let in_path = in_path.as_ref();
    let out = out.as_ref();

    // 0. 容器/编码门禁（单 XML + UTF-8 才可安全字节改写）
    let bytes = std::fs::read(in_path)
        .map_err(|e| DocError::corrupt(format!("cannot read {}: {e}", in_path.display())))?;
    if bytes.starts_with(b"PK\x03\x04") {
        return Err(DocError::corrupt(
            "fb2.zip 容器回写不支持：请对解包出的内层 .fb2 单 XML 件回写",
        ));
    }
    if std::str::from_utf8(&bytes).is_err() {
        return Err(DocError::corrupt(
            "fb2 非 UTF-8：回写仅支持 UTF-8 件（避免 XML 声明编码失配）",
        ));
    }

    // A. 权威解析（提取侧同管线、同默认选项）拿 id / 文本
    let raw = Fb2RawBackend::default().parse(in_path)?;
    let doc = Fb2Plugin.assemble(&raw);
    let units: Vec<_> = crate::emit::to_translation_units(&doc)
        .into_iter()
        // 表格单元格单元（kind = Table）恒久不进 fb2 回写：fb2 v1 不承诺
        // 表格回写（`<table>` 产 Table 块占槽位但不占翻译位，见模块文档），
        // 单元流同步排除，表格在 XML 里保持原样。
        .filter(|u| u.kind != crate::model::BlockKind::Table)
        .collect();
    let map = translation_map(translated);

    // B. 主文 body 重扫描（提取侧遍历的镜像，带字节锚点）
    let xml = String::from_utf8(bytes).expect("UTF-8 已校验");
    let scanned = scan_fb2(&xml)?;
    if doc.pages.len() < scanned.pages.len() {
        return Err(DocError::corrupt(
            "fb2 对齐失败：Document 页数少于重扫描页数",
        ));
    }

    // C. 页 × 块 × 单元按序配对（同一过滤口径）+ 一致性守卫，逐策略产编辑
    let mut edits: Vec<Edit> = Vec::new();
    let mut matched: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    for (page_idx, page) in doc.pages.iter().enumerate() {
        // 超出主文页（脚注尾页等）：Note 块不进翻译流，直接跳过
        let Some(slots) = scanned.pages.get(page_idx) else {
            break;
        };
        if slots.len() != page.blocks.len() {
            return Err(DocError::corrupt(
                "fb2 对齐失败：重扫描槽位数与解析块数不一致",
            ));
        }
        for (b_idx, b) in page.blocks.iter().enumerate() {
            let t = b.content.trim();
            if !crate::emit::is_translatable(b.kind) || t.is_empty() || t.chars().count() < 2 {
                continue;
            }
            if cursor >= units.len() {
                break;
            }
            let unit = &units[cursor];
            cursor += 1;
            let slot = &slots[b_idx];
            // 双保险：重扫描文本必须与权威单元文本一致
            if slot.text != unit.text {
                continue;
            }
            let Some(tr) = map.get(unit.id.as_str()) else {
                continue;
            };
            let tr = tr.trim();
            if tr.is_empty() {
                continue;
            }
            matched.push(tr.to_string());
            match strategy {
                BiStrategy::BilingualInline => inline_edit(&slot.anchor, tr, &mut edits),
                BiStrategy::TranslationOnly => tonly_edits(&slot.anchor, tr, &mut edits),
                BiStrategy::OriginalPlusAppendix => {}
            }
        }
    }

    // Appendix：正文不动，主文 </body> 前追加「译文」section（重解析自开新页）
    if strategy == BiStrategy::OriginalPlusAppendix && !matched.is_empty() {
        if let Some(pos) = scanned.body_close {
            let mut appendix = String::from("<section><title><p>译文</p></title>");
            for tr in &matched {
                appendix.push_str(&format!("<p>{}</p>", escape_xml_text(tr)));
            }
            appendix.push_str("</section>");
            edits.push(Edit::Insert(pos, appendix));
        }
    }

    // D. 改写并写回（无命中 = 原文原样）
    let out_xml = if edits.is_empty() {
        xml
    } else {
        apply_edits(&xml, edits)
    };
    std::fs::write(out, out_xml.into_bytes())
        .map_err(|e| DocError::corrupt(format!("cannot create {}: {e}", out.display())))
}

/// BilingualInline 的锚点编辑：能在同级合法放 `<p>` 的锚点才插（宁漏勿错）。
fn inline_edit(anchor: &Anchor, tr: &str, edits: &mut Vec<Edit>) {
    let esc = escape_xml_text(tr);
    match anchor {
        Anchor::Inline { after, p_ok, .. } if *p_ok => {
            edits.push(Edit::Insert(*after, format!("<p>{esc}</p>")));
        }
        Anchor::Title { after, p_ok, .. } if *p_ok => {
            edits.push(Edit::Insert(*after, format!("<p>{esc}</p>")));
        }
        _ => {} // poem 内部（stanza / text-author / poem 标题）或无锚：跳过
    }
}

/// TranslationOnly 的锚点编辑：Inline 换元素内文本；Title/Stanza 首 span 换
/// 译文、其余清空（保留元素结构）。
fn tonly_edits(anchor: &Anchor, tr: &str, edits: &mut Vec<Edit>) {
    match anchor {
        Anchor::Inline { inner, .. } => {
            let (s, e) = *inner;
            if s < e {
                edits.push(Edit::Replace(s, e, escape_xml_text(tr)));
            }
        }
        Anchor::Title { spans, .. } | Anchor::Stanza { spans } => {
            for (k, (s, e)) in spans.iter().enumerate() {
                if s >= e {
                    continue;
                }
                if k == 0 {
                    edits.push(Edit::Replace(*s, *e, escape_xml_text(tr)));
                } else {
                    edits.push(Edit::Replace(*s, *e, String::new()));
                }
            }
        }
        Anchor::None => {}
    }
}

// ---------------------------------------------------------------------------
// 主文 body 重扫描（提取侧遍历的镜像，带字节锚点）
// ---------------------------------------------------------------------------

/// 一个 item 的重扫描定位：文本（提取侧同口径 trim）+ 字节锚点。
struct Fb2Slot {
    text: String,
    anchor: Anchor,
}

/// 锚点形态（见模块级文档的定位锚点表）。
enum Anchor {
    /// p / subtitle / text-author：`after` = 元素 End 后偏移（Inline 插位）、
    /// `inner` = 元素内文本区（Only 替换区）、`p_ok` = 同级放 `<p>` 是否合法。
    Inline {
        after: usize,
        inner: (usize, usize),
        p_ok: bool,
    },
    /// `<title>`：`spans` = 各内层 `<p>` 的文本区；`after` = `</title>` 后偏移；
    /// `p_ok` = title 之后放 `<p>` 是否合法（body/section true，poem false）。
    Title {
        after: usize,
        spans: Vec<(usize, usize)>,
        p_ok: bool,
    },
    /// `<stanza>`：各 `<v>` 的文本区（Inline 不做；Only 首 v 换其余清）。
    Stanza { spans: Vec<(usize, usize)> },
    /// Table 等不进翻译流的 item：占槽位，无锚点。
    None,
}

/// 重扫描结果：页序列（卷首? + section*，与 raw.pages 前缀同位）+ 主文
/// `</body>` 起始偏移。
struct Scanned {
    pages: Vec<Vec<Fb2Slot>>,
    body_close: Option<usize>,
}

/// quick-xml 0.42 的 `buffer_position()` 是 `u64`；本模块 span 统一 `usize`。
fn bpos<R: std::io::BufRead>(reader: &Reader<R>) -> usize {
    reader.buffer_position() as usize
}

/// Text 事件取内容并解实体（提取侧 `text_of` 同款）。
fn text_of(e: &quick_xml::events::BytesText<'_>) -> Option<String> {
    let raw = e.xml10_content();
    quick_xml::escape::unescape(&raw)
        .ok()
        .map(|v| v.into_owned())
}

/// GeneralRef 就地解码（提取侧 `push_ref` 同款）。
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

/// 跳过当前子树（提取侧 `skip_subtree` 同款）。
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

/// body 的 `name` 属性（无 / 空 = 主文 body；提取侧同口径）。
fn body_name(e: &BytesStart<'_>) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == "name")
        .map(|a| a.value.to_string())
        .filter(|s| !s.is_empty())
}

/// 槽位登记（提取侧 `push_item` 同口径：trim 非空才产）。
fn push_slot(slots: &mut Vec<Fb2Slot>, text: String, anchor: Anchor) {
    let t = text.trim();
    if t.is_empty() {
        return;
    }
    slots.push(Fb2Slot {
        text: t.to_string(),
        anchor,
    });
}

/// 整份 XML → 主文 body 的槽位页（FictionBook → 首个无 name 的 body）。
/// 与 [`crate::backends::fb2::parse_fb2`] 同款遍历：首个无 name body 为主文
/// （其前的 named body 视为脚注、跳过），其余兄弟元素整棵跳过。
fn scan_fb2(xml: &str) -> Result<Scanned> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut seen_root = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().local_name().as_ref().to_string();
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
                if name == "body" {
                    match body_name(e) {
                        // 首个无 name 的 body 即主文（named body 是脚注，跳过；
                        // 与提取侧 parse_fb2 同款规则——回写侧遇主文即进扫描）
                        None => return scan_main_body(&mut reader),
                        _ => skip_subtree(&mut reader),
                    }
                } else {
                    skip_subtree(&mut reader);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("FB2 XML 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Err(DocError::corrupt("FB2 缺正文 body（无主文 body）"))
}

/// 主文 body → 槽位页（卷首 + 顶层 section 各一页；提取侧 `parse_main_body`
/// 镜像）。空 section 照样成页（提取侧同口径，保证与 doc.pages 同位）。
fn scan_main_body(reader: &mut Reader<&[u8]>) -> Result<Scanned> {
    let mut preamble: Vec<Fb2Slot> = Vec::new();
    let mut sections: Vec<Vec<Fb2Slot>> = Vec::new();
    let mut buf = Vec::new();
    let mut body_close = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "title" => {
                        let t = scan_title(reader)?;
                        push_slot(
                            &mut preamble,
                            t.text,
                            Anchor::Title {
                                after: t.after,
                                spans: t.spans,
                                p_ok: true,
                            },
                        );
                    }
                    "section" => {
                        let mut items = Vec::new();
                        scan_section(reader, 1, &mut items)?;
                        sections.push(items);
                    }
                    "p" => {
                        let (text, inner, after) = scan_inline_elem(reader, "p")?;
                        push_slot(
                            &mut preamble,
                            text,
                            Anchor::Inline {
                                after,
                                inner,
                                p_ok: true,
                            },
                        );
                    }
                    "poem" => scan_poem(reader, &mut preamble)?,
                    "table" => push_table_slot(reader, &mut preamble)?,
                    "epigraph" | "cite" => {
                        scan_flow(reader, n.as_ref(), 1, &mut preamble)?
                    }
                    // 内联图片引用不产 item（提取侧同口径：连子树都不消费）
                    "image" => {}
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "body" => {
                body_close = Some(bpos(reader) - "</body>".len());
                break;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("FB2 body 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    let mut pages = Vec::new();
    if !preamble.is_empty() {
        pages.push(preamble);
    }
    for items in sections {
        pages.push(items);
    }
    Ok(Scanned { pages, body_close })
}

/// `<section>`（可嵌套；提取侧 `parse_section` 镜像）：title 按嵌套深度、
/// subtitle 深一级，嵌套 section 的槽位并入当前页。
fn scan_section(reader: &mut Reader<&[u8]>, depth: u8, slots: &mut Vec<Fb2Slot>) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "title" => {
                        let t = scan_title(reader)?;
                        push_slot(
                            slots,
                            t.text,
                            Anchor::Title {
                                after: t.after,
                                spans: t.spans,
                                p_ok: true,
                            },
                        );
                    }
                    "subtitle" => {
                        let (text, inner, after) = scan_inline_elem(reader, "subtitle")?;
                        push_slot(
                            slots,
                            text,
                            Anchor::Inline {
                                after,
                                inner,
                                p_ok: true,
                            },
                        );
                    }
                    "section" => {
                        if depth < MAX_SECTION_DEPTH {
                            scan_section(reader, depth.saturating_add(1), slots)?;
                        } else {
                            skip_subtree(reader); // 病态嵌套：丢弃而非栈溢出
                        }
                    }
                    "p" => {
                        let (text, inner, after) = scan_inline_elem(reader, "p")?;
                        push_slot(
                            slots,
                            text,
                            Anchor::Inline {
                                after,
                                inner,
                                p_ok: true,
                            },
                        );
                    }
                    "poem" => scan_poem(reader, slots)?,
                    "table" => push_table_slot(reader, slots)?,
                    "epigraph" | "cite" => {
                        scan_flow(reader, n.as_ref(), 1, slots)?
                    }
                    "image" => {}
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "section" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("FB2 section 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// epigraph / cite 容器（提取侧 `flow_until` 镜像）：内部按流式元素展开；
/// 容器内 p / subtitle / text-author 的同级 `<p>` 合法（p_ok = true）。
fn scan_flow(reader: &mut Reader<&[u8]>, end: &str, depth: u8, slots: &mut Vec<Fb2Slot>) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "p" | "subtitle" | "text-author" => {
                        let tag = n.as_ref().to_string();
                        let (text, inner, after) = scan_inline_elem(reader, &tag)?;
                        push_slot(
                            slots,
                            text,
                            Anchor::Inline {
                                after,
                                inner,
                                p_ok: true,
                            },
                        );
                    }
                    "poem" => scan_poem(reader, slots)?,
                    "table" => push_table_slot(reader, slots)?,
                    "epigraph" | "cite" => {
                        if depth < MAX_FLOW_DEPTH {
                            let tag = n.as_ref().to_string();
                            scan_flow(reader, &tag, depth + 1, slots)?;
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
            Err(e) => return Err(DocError::corrupt(format!("FB2 {end} 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// `<poem>`（提取侧 `poem_until` 镜像）：stanza 一段（v 行间 `\n`）、
/// text-author 单独成段、诗题按普通段落。poem 内不允许 `<p>`：
/// 内部锚点 Inline p_ok = false、Title p_ok = false、Stanza 无 Inline。
fn scan_poem(reader: &mut Reader<&[u8]>, slots: &mut Vec<Fb2Slot>) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "title" => {
                        let t = scan_title(reader)?;
                        push_slot(
                            slots,
                            t.text,
                            Anchor::Title {
                                after: t.after,
                                spans: t.spans,
                                p_ok: false,
                            },
                        );
                    }
                    "stanza" => {
                        let (text, spans) = scan_stanza(reader)?;
                        push_slot(slots, text, Anchor::Stanza { spans });
                    }
                    "text-author" => {
                        let (text, inner, after) = scan_inline_elem(reader, "text-author")?;
                        push_slot(
                            slots,
                            text,
                            Anchor::Inline {
                                after,
                                inner,
                                p_ok: false,
                            },
                        );
                    }
                    "epigraph" | "cite" => {
                        // 容器自身允许 p：内部锚点 p_ok = true（scan_flow 默认）
                        let tag = n.as_ref().to_string();
                        scan_flow(reader, &tag, 1, slots)?
                    }
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "poem" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("FB2 poem 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// `<stanza>` 的 `<v>` 行（提取侧 `stanza_lines` 镜像）：文本 = 非空 trim 行
/// 的 `\n` 连接；spans = 全部 `<v>` 的内部文本区（Only 首换余清用）。
fn scan_stanza(reader: &mut Reader<&[u8]>) -> Result<(String, Vec<(usize, usize)>)> {
    let mut lines: Vec<String> = Vec::new();
    let mut spans = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "v" => {
                        let (text, inner, _after) = scan_inline_elem(reader, "v")?;
                        spans.push(inner);
                        if !text.trim().is_empty() {
                            lines.push(text.trim().to_string());
                        }
                    }
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "stanza" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("FB2 stanza 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    let joined = lines.join("\n");
    let text = joined.trim().to_string();
    if text.is_empty() {
        return Ok((String::new(), Vec::new())); // push_item 同口径：空 → 不产槽位
    }
    Ok((text, spans))
}

/// `<table>` → 槽位（提取侧 `push_table` 镜像：空表不产 item；Table 块不进
/// 翻译流，锚点 None 只占位）。
fn push_table_slot(reader: &mut Reader<&[u8]>, slots: &mut Vec<Fb2Slot>) -> Result<()> {
    let rows = scan_table_rows(reader)?;
    if !rows.is_empty() {
        slots.push(Fb2Slot {
            text: String::new(),
            anchor: Anchor::None,
        });
    }
    Ok(())
}

/// `<table>` → rows × cells（提取侧 `table_rows` 镜像；行文本不参与回写匹配）。
fn scan_table_rows(reader: &mut Reader<&[u8]>) -> Result<Vec<Vec<String>>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let n = e.name().local_name();
                match n.as_ref() {
                    "tr" => {
                        let mut cells = Vec::new();
                        let mut rbuf = Vec::new();
                        loop {
                            match reader.read_event_into(&mut rbuf) {
                                Ok(Event::Start(ref c)) => {
                                    let cn = c.name().local_name();
                                    if matches!(cn.as_ref(), "td" | "th") {
                                        let tag = cn.as_ref().to_string();
                                        let (text, _inner, _after) =
                                            scan_inline_elem(reader, &tag)?;
                                        cells.push(text.trim().to_string());
                                    } else {
                                        skip_subtree(reader);
                                    }
                                }
                                Ok(Event::Empty(ref c)) => {
                                    let cn = c.name().local_name();
                                    if matches!(cn.as_ref(), "td" | "th") {
                                        cells.push(String::new());
                                    }
                                }
                                Ok(Event::End(ref c))
                                    if c.name().local_name().as_ref() == "tr" =>
                                {
                                    break;
                                }
                                Ok(Event::Eof) => break,
                                Err(e) => {
                                    return Err(DocError::corrupt(format!(
                                        "FB2 tr 解析失败: {e}"
                                    )))
                                }
                                _ => {}
                            }
                            rbuf.clear();
                        }
                        rows.push(cells);
                    }
                    _ => skip_subtree(reader),
                }
            }
            Ok(Event::Empty(_)) => {}
            Ok(Event::End(ref e)) if e.name().local_name().as_ref() == "table" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("FB2 table 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(rows)
}

/// 段落内联元素（p / subtitle / text-author / v）：文本与提取侧 `inline_until`
/// 同口径（内联标记透明拍平、`<empty-line/>` 折行），附带内文本区与 End 后
/// 偏移。返回 (原始文本, (inner_start, inner_end), after)。
fn scan_inline_elem<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    end: &str,
) -> Result<(String, (usize, usize), usize)> {
    let inner_start = bpos(reader);
    let mut out = String::new();
    let mut buf = Vec::new();
    let mut depth = 1usize;
    let (mut inner_end, mut after) = (inner_start, inner_start);
    loop {
        let pos_before = bpos(reader);
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
                        inner_end = pos_before;
                        after = bpos(reader);
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
            Err(e) => return Err(DocError::corrupt(format!("FB2 {end} 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok((out, (inner_start, inner_end), after))
}

/// `<title>` 的扫描结果：归一化文本 + 各内层 `<p>` 的内部文本区 +
/// `</title>` 后偏移。
struct TitleScan {
    text: String,
    spans: Vec<(usize, usize)>,
    after: usize,
}

/// `<title>` 文本（提取侧 `title_text` 镜像：各段文本空格连接后归一化空白），
/// 附带各内层 `<p>` 的内部文本区与 `</title>` 后偏移。
fn scan_title<R: std::io::BufRead>(reader: &mut Reader<R>) -> Result<TitleScan> {
    let mut out = String::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut cur: Option<(usize, usize)> = None;
    let mut buf = Vec::new();
    let mut after = 0usize;
    loop {
        let pos_before = bpos(reader);
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.name().local_name().as_ref() == "p" {
                    cur = Some((bpos(reader), 0));
                }
            }
            Ok(Event::End(ref e)) => match e.name().local_name().as_ref() {
                "title" => {
                    after = bpos(reader);
                    break;
                }
                "p" => {
                    if let Some(mut span) = cur.take() {
                        span.1 = pos_before;
                        if span.0 < span.1 {
                            spans.push(span);
                        }
                    }
                    out.push(' ');
                }
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
            Err(e) => return Err(DocError::corrupt(format!("FB2 title 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    let text = out.split_whitespace().collect::<Vec<_>>().join(" ");
    Ok(TitleScan { text, spans, after })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::path::PathBuf;

    use crate::emit::to_translation_units;
    use crate::model::BlockKind::{Heading, Note, Paragraph};

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_fb2_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn book_xml() -> String {
        r#"<?xml version="1.0" encoding="utf-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/FictionBook/2.0">
<description><title-info><book-title>测试之书</book-title><lang>zh</lang></title-info></description>
<body>
  <title><p>测试之书</p></title>
  <epigraph><p>卷首语。</p></epigraph>
  <section id="ch1">
    <title><p>第一章</p></title>
    <p>第一章正文一段。</p>
    <p>第一章正文二段。</p>
    <section>
      <title><p>第一节</p></title>
      <p>第一节正文。</p>
    </section>
    <subtitle>节尾小结</subtitle>
  </section>
  <section id="ch2">
    <title><p>第二章</p></title>
    <poem>
      <stanza><v>白日依山尽</v><v>黄河入海流</v></stanza>
    </poem>
    <p>第二章尾段。</p>
  </section>
</body>
<body name="notes">
  <section id="n1"><title><p>1</p></title><p>第一条脚注</p></section>
</body>
</FictionBook>"#
            .to_string()
    }

    /// 提取侧同管线（Fb2RawBackend + Fb2Plugin）。
    fn parse_doc(path: &Path) -> crate::model::Document {
        let raw = Fb2RawBackend::default()
            .parse(path)
            .unwrap();
        Fb2Plugin.assemble(&raw)
    }

    fn unit_ids(path: &Path) -> Vec<String> {
        to_translation_units(&parse_doc(path))
            .into_iter()
            .map(|u| u.id)
            .collect()
    }

    fn write(in_path: &Path, out_path: &Path, tr: &[(&str, &str)], strategy: BiStrategy) {
        let translated: Vec<(String, String)> =
            tr.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        writeback_fb2(in_path, &translated, strategy, out_path).unwrap();
    }

    /// 金测（核心验收）：BilingualInline 往返 —— 卷首/章/嵌套节/小标题的
    /// 原/译交错（title 译插 </title> 后、subtitle 后插 p）、poem stanza 不插
    /// （poem 内不允许 p）、脚注 body 不受扰、未匹配 id 忽略。
    #[test]
    fn roundtrip_bilingual_inline() {
        let dir = case_dir("inline");
        let in_path = dir.join("in.fb2");
        std::fs::write(&in_path, book_xml()).unwrap();

        // 提取侧 id 口径（页名 = 首个一级标题；嵌套节 title/stanza 都是候选）
        assert_eq!(
            unit_ids(&in_path),
            [
                "p测试之书-b0",
                "p测试之书-b1",
                "p第一章-b0",
                "p第一章-b1",
                "p第一章-b2",
                "p第一章-b3",
                "p第一章-b4",
                "p第一章-b5",
                "p第二章-b0",
                "p第二章-b1",
                "p第二章-b2",
            ]
        );

        let out_path = dir.join("out.fb2");
        write(
            &in_path,
            &out_path,
            &[
                ("p测试之书-b0", "Book of Tests"),
                ("p测试之书-b1", "Epigraph text."),
                ("p第一章-b0", "Chapter 1"),
                ("p第一章-b1", "First paragraph."),
                ("p第一章-b3", "Section 1"),
                ("p第一章-b4", "Nested body."),
                ("p第一章-b5", "Section recap."),
                ("p第二章-b0", "Chapter 2"),
                ("p第二章-b2", "Final paragraph."),
                ("p不存在-b0", "bogus"), // 未匹配 id：忽略
            ],
            BiStrategy::BilingualInline,
        );

        let doc = parse_doc(&out_path);
        let names: Vec<Option<&str>> = doc.pages.iter().map(|p| p.name.as_deref()).collect();
        assert_eq!(
            names,
            [Some("测试之书"), Some("第一章"), Some("第二章"), Some("notes")]
        );

        // 卷首页：title 译插 </title> 后；epigraph 内 p 译插其后（容器内合法）
        let p0 = &doc.pages[0];
        assert_eq!(
            p0.blocks.iter().map(|b| (b.kind, b.content.as_str())).collect::<Vec<_>>(),
            [
                (Heading, "测试之书"),
                (Paragraph, "Book of Tests"),
                (Paragraph, "卷首语。"),
                (Paragraph, "Epigraph text."),
            ]
        );

        // 第一章页：title 译 + 段落译交错；嵌套节 title 译插其 </title> 后；
        // subtitle 译插其后
        let p1 = &doc.pages[1];
        assert_eq!(
            p1.blocks.iter().map(|b| (b.kind, b.content.as_str())).collect::<Vec<_>>(),
            [
                (Heading, "第一章"),
                (Paragraph, "Chapter 1"),
                (Paragraph, "第一章正文一段。"),
                (Paragraph, "First paragraph."),
                (Paragraph, "第一章正文二段。"),
                (Heading, "第一节"),
                (Paragraph, "Section 1"),
                (Paragraph, "第一节正文。"),
                (Paragraph, "Nested body."),
                (Heading, "节尾小结"),
                (Paragraph, "Section recap."),
            ]
        );

        // 第二章页：stanza 无译文（poem 内不允许 p，宁漏勿错）；尾段有译
        let p2 = &doc.pages[2];
        assert_eq!(
            p2.blocks.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            ["第二章", "Chapter 2", "白日依山尽\n黄河入海流", "第二章尾段。", "Final paragraph."]
        );

        // 脚注尾页不受扰；未匹配 id 不落盘
        let notes = &doc.pages[3];
        assert_eq!(notes.blocks[0].kind, Note);
        assert_eq!(notes.blocks[0].content, "1\n\n第一条脚注");
        assert!(!std::fs::read_to_string(&out_path).unwrap().contains("bogus"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TranslationOnly：title 首 p 换译、p 元素内文本替换、stanza 首 v 换译
    /// 其余 v 清空、未翻译单元原样。
    #[test]
    fn translation_only_replaces_text() {
        let dir = case_dir("tonly");
        let in_path = dir.join("in.fb2");
        std::fs::write(&in_path, book_xml()).unwrap();
        let out_path = dir.join("out.fb2");
        write(
            &in_path,
            &out_path,
            &[
                ("p测试之书-b0", "Book of Tests"),
                ("p第一章-b1", "First paragraph."),
                ("p第一章-b4", "Nested body."),
                ("p第二章-b1", "One couplet."),
            ],
            BiStrategy::TranslationOnly,
        );

        let doc = parse_doc(&out_path);
        // 卷首：title 首个 <p> 换译文（title 仍是单段标题）；epigraph 原样
        let p0 = &doc.pages[0];
        assert_eq!(p0.blocks[0].content, "Book of Tests");
        assert_eq!(p0.blocks[1].content, "卷首语。");
        // 第一章：指定段替换、其余原样
        let p1 = &doc.pages[1];
        assert_eq!(
            p1.blocks.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            [
                "第一章",
                "First paragraph.",
                "第一章正文二段。",
                "第一节",
                "Nested body.",
                "节尾小结",
            ]
        );
        // 第二章：stanza 首 v 换译文、其余 v 清空（重解析 = 单行译文）
        let p2 = &doc.pages[2];
        assert_eq!(
            p2.blocks.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            ["第二章", "One couplet.", "第二章尾段。"]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：正文不动，主文 </body> 前追加「译文」section
    /// （重解析自开新页）；全部未匹配时输出逐字节透传。
    #[test]
    fn appendix_appends_before_body_close() {
        let dir = case_dir("appendix");
        let in_path = dir.join("in.fb2");
        std::fs::write(&in_path, book_xml()).unwrap();
        let out_path = dir.join("out.fb2");
        write(
            &in_path,
            &out_path,
            &[
                ("p测试之书-b1", "Epigraph text."),
                ("p第一章-b1", "First paragraph."),
                ("p不存在-b9", "bogus"),
            ],
            BiStrategy::OriginalPlusAppendix,
        );

        let out = std::fs::read_to_string(&out_path).unwrap();
        let body_end = out.rfind("</body>").unwrap();
        let app = out.find("<section><title><p>译文</p></title>").unwrap();
        assert!(app < body_end, "附录在主文 </body> 之前");
        assert!(out.contains("卷首语。"), "正文不动");

        // 重解析：原 3 页不动 + 译文自开新页（脚注尾页仍在最后——附录
        // section 是主文的新顶层 section，页序在 notes 之前）
        let doc = parse_doc(&out_path);
        assert_eq!(doc.pages.len(), 5);
        assert_eq!(doc.pages[3].name.as_deref(), Some("译文"));
        assert_eq!(
            doc.pages[3]
                .blocks
                .iter()
                .map(|b| (b.kind, b.content.as_str()))
                .collect::<Vec<_>>(),
            [(Heading, "译文"), (Paragraph, "Epigraph text."), (Paragraph, "First paragraph.")]
        );
        assert_eq!(doc.pages[4].name.as_deref(), Some("notes"), "脚注尾页仍在最后");

        // 全部未匹配：逐字节透传
        let out2 = dir.join("out2.fb2");
        write(&in_path, &out2, &[("p不存在-b42", "nope")], BiStrategy::BilingualInline);
        assert_eq!(std::fs::read(&out2).unwrap(), book_xml().into_bytes());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 容器/编码门禁：fb2.zip 拒绝、非 UTF-8 拒绝。
    #[test]
    fn container_and_encoding_guards() {
        let dir = case_dir("guards");

        // fb2.zip：拒绝（解析侧能解包读，回写侧不做容器重打包）
        let zip_path = dir.join("book.fb2.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            zw.start_file("inner.fb2", zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(book_xml().as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let err = writeback_fb2(
            &zip_path,
            &[("p第一章-b1".to_string(), "x".to_string())],
            BiStrategy::BilingualInline,
            dir.join("zip-out.fb2"),
        )
        .unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(err.message.contains("zip"));

        // 非 UTF-8：拒绝（避免 XML 声明编码失配）
        let bad = dir.join("bad.fb2");
        std::fs::write(&bad, [0xFF, 0xFE, b'<', b'x']).unwrap();
        let err = writeback_fb2(
            &bad,
            &[],
            BiStrategy::BilingualInline,
            dir.join("bad-out.fb2"),
        )
        .unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(err.message.contains("UTF-8"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
