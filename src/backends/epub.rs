// 吸收自 docparser-epub 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：crate 引用重写；B 五件套融合（资源提取/封面 fallback/linear=no/编码嗅探）
// 列为 M2b-2 增补（docparse-plan §2.4-2），本文件先平移 A 主干。
//! `docparser-epub`：EPUB2/EPUB3 后端。
//!
//! 结构（对齐 Python `book.read_epub`）：
//! `META-INF/container.xml` → rootfile（OPF 路径）→ `metadata` / `manifest` / `spine`，
//! 章节标题优先取 EPUB3 `nav`，其次 EPUB2 `toc.ncx`，取不到回落到文档内首个标题；
//! 封面取 `properties="cover-image"`，其次 `<meta name="cover">`。
//!
//! 正文：每个 spine 文档用 `crate::plugins::html` 解析成**结构化块**
//! （标题/段落/列表/引用/代码/表格），再按文档内标题切节（层级自适应 h2→h3→h4，
//! 单节超过 [`BIG_SECTION_CHARS`] 才往深一层切）——与 Python 的分章口径一致，
//! 区别是这里保留块结构而不是压平成一段文本。
//!
//! 不做的事：DRM（`META-INF/encryption.xml`）不解密，加密书会在读取正文时报 `corrupt`；
//! 图片/字体等资源不解包（只记录封面路径）。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use quick_xml::events::{BytesEnd, BytesStart, Event};
use quick_xml::Reader;

use crate::error::{DocError, Result};
use crate::model::{Block, BlockKind, Document, Page};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawPage, RawTextItem};
use crate::plugins::html::parse_blocks;
use crate::registry::Registry;

/// 单节超过该字符数就尝试按更深层标题再切（与 Python `BIG_SECTION_CHARS` 一致）。
pub const BIG_SECTION_CHARS: usize = 8000;
const SECTION_LEVELS: [u8; 3] = [2, 3, 4];

/// 书籍元数据（取不到的字段为空串 / None）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpubMeta {
    pub title: String,
    pub authors: Vec<String>,
    pub language: String,
    pub publisher: String,
    /// 从 `dc:date` 里取的四位年份。
    pub year: String,
    /// 封面在包内的路径（POSIX，相对包根）。
    pub cover_href: Option<String>,
}

/// 一章（= 一个 spine 文档，或文档内按标题切出的一节）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EpubChapter {
    pub title: String,
    pub blocks: Vec<Block>,
    /// `blocks` 的非空 content 以空行连接（与 Python 章文本口径一致）。
    pub text: String,
}

/// 整本书的解析结果。
#[derive(Debug, Clone, Default)]
pub struct EpubBook {
    pub meta: EpubMeta,
    pub chapters: Vec<EpubChapter>,
}

// ── XML 小工具 ───────────────────────────────────────────────────────

fn local_name(e: &BytesStart<'_>) -> String {
    e.local_name().as_ref().to_string()
}

fn end_name(e: &BytesEnd<'_>) -> String {
    e.local_name().as_ref().to_string()
}

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
    DocError::corrupt(format!("{what} 解析失败: {e}"))
}

// ── 容器 / OPF ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct Item {
    id: String,
    href: String,
    props: String,
    /// manifest media-type（docforge 五件套：图片提取按 image/* 过滤）
    media_type: String,
}

/// spine 引用（docforge 五件套：linear="no" 跳过，缺省视为 yes）。
#[derive(Debug, Clone)]
struct SpineRef {
    idref: String,
    linear: bool,
}

#[derive(Debug, Clone, Default)]
struct Opf {
    title: String,
    authors: Vec<String>,
    language: String,
    publisher: String,
    date: String,
    items: Vec<Item>,
    spine: Vec<SpineRef>,
    toc: Option<String>,
    cover_id: Option<String>,
}

/// `container.xml` → OPF 包内路径。（writeback::epub 同口径引用。）
pub(crate) fn find_rootfile(xml: &str) -> Result<Option<String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if local_name(&e) == "rootfile" {
                    if let Some(p) = attrs(&e).get("full-path") {
                        return Ok(Some(p.clone()));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("container.xml", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(None)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Creator,
    Language,
    Publisher,
    Date,
}

/// OPF 单元素处理：元数据字段登记 + `manifest` / `spine` / 封面收集。
///
/// `is_empty`（自闭合元素）不登记文本字段——没有正文可收。
fn on_opf_element(
    opf: &mut Opf,
    e: &BytesStart<'_>,
    stack: &[String],
    field: &mut Option<Field>,
    acc: &mut String,
    is_empty: bool,
) {
    let name = local_name(e);
    let a = attrs(e);
    let in_metadata = stack.iter().any(|s| s == "metadata");
    if in_metadata && field.is_none() && !is_empty {
        let f = match name.as_str() {
            "title" => Some(Field::Title),
            "creator" => Some(Field::Creator),
            "language" => Some(Field::Language),
            "publisher" => Some(Field::Publisher),
            "date" => Some(Field::Date),
            _ => None,
        };
        if f.is_some() {
            *field = f;
            acc.clear();
        }
    }
    match name.as_str() {
        "item" => opf.items.push(Item {
            id: a.get("id").cloned().unwrap_or_default(),
            href: a.get("href").cloned().unwrap_or_default(),
            props: a.get("properties").cloned().unwrap_or_default(),
            media_type: a.get("media-type").cloned().unwrap_or_default(),
        }),
        "itemref" => {
            if let Some(id) = a.get("idref") {
                // docforge 五件套：linear="no" 的项不进阅读流（B 语义）
                let linear = a.get("linear").map(|v| v != "no").unwrap_or(true);
                opf.spine.push(SpineRef { idref: id.clone(), linear });
            }
        }
        "spine" => {
            if let Some(t) = a.get("toc") {
                opf.toc = Some(t.clone());
            }
        }
        "meta" if a.get("name").map(|n| n == "cover").unwrap_or(false) => {
            opf.cover_id = a.get("content").cloned();
        }
        _ => {}
    }
}

fn parse_opf(xml: &str) -> Result<Opf> {
    let mut opf = Opf::default();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut field: Option<Field> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                on_opf_element(&mut opf, &e, &stack, &mut field, &mut acc, false);
                stack.push(local_name(&e));
            }
            Ok(Event::Empty(e)) => {
                on_opf_element(&mut opf, &e, &stack, &mut field, &mut acc, true);
            }
            Ok(Event::Text(t)) => {
                if field.is_some() {
                    if let Some(v) = text_of(&t) {
                        acc.push_str(&v);
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if field.is_some() {
                    push_ref(&mut acc, e);
                }
            }
            Ok(Event::End(e)) => {
                let name = end_name(&e);
                if let Some(f) = field {
                    let matched = matches!(
                        (f, name.as_str()),
                        (Field::Title, "title")
                            | (Field::Creator, "creator")
                            | (Field::Language, "language")
                            | (Field::Publisher, "publisher")
                            | (Field::Date, "date")
                    );
                    if matched {
                        let v = acc.trim().to_string();
                        match f {
                            Field::Title => {
                                if opf.title.is_empty() {
                                    opf.title = v;
                                }
                            }
                            Field::Creator => {
                                if !v.is_empty() {
                                    opf.authors.push(v);
                                }
                            }
                            Field::Language => {
                                if opf.language.is_empty() {
                                    opf.language = v;
                                }
                            }
                            Field::Publisher => {
                                if opf.publisher.is_empty() {
                                    opf.publisher = v;
                                }
                            }
                            Field::Date => {
                                if opf.date.is_empty() {
                                    opf.date = v;
                                }
                            }
                        }
                        field = None;
                        acc.clear();
                    }
                }
                if stack.last().map(|s| s == &name).unwrap_or(false) {
                    stack.pop();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(xml_err("OPF", e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(opf)
}

/// EPUB3 nav.xhtml → {href(去 fragment): 标题}。
fn nav_titles(xml: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut href: Option<String> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if local_name(&e) == "a" {
                    href = attrs(&e).get("href").cloned();
                    acc.clear();
                }
            }
            Ok(Event::Text(t)) => {
                if href.is_some() {
                    if let Some(v) = text_of(&t) {
                        acc.push_str(&v);
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if href.is_some() {
                    push_ref(&mut acc, e);
                }
            }
            Ok(Event::End(e)) => {
                if end_name(&e) == "a" {
                    if let Some(h) = href.take() {
                        let h = h.split('#').next().unwrap_or("").to_string();
                        let title = acc.trim().to_string();
                        if !h.is_empty() && !title.is_empty() {
                            out.entry(h).or_insert(title);
                        }
                    }
                    acc.clear();
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// EPUB2 toc.ncx → {href(去 fragment): 标题}（`navLabel/text` + `content@src` 配对）。
fn ncx_titles(xml: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut last_label = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = local_name(&e);
                match name.as_str() {
                    "text" => {
                        in_text = true;
                        last_label.clear();
                    }
                    "content" => {
                        if let Some(src) = attrs(&e).get("src") {
                            let h = src.split('#').next().unwrap_or("").to_string();
                            if !h.is_empty() && !last_label.is_empty() {
                                out.entry(h).or_insert_with(|| last_label.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if in_text {
                    if let Some(v) = text_of(&t) {
                        last_label.push_str(&v);
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if in_text {
                    push_ref(&mut last_label, e);
                }
            }
            Ok(Event::End(e)) => {
                if end_name(&e) == "text" {
                    last_label = last_label.trim().to_string();
                    in_text = false;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

// ── 路径 / ZIP ──────────────────────────────────────────────────────

/// manifest href（可含 `./` `../`）→ 包内 POSIX 路径。（writeback::epub 同口径引用。）
pub(crate) fn resolve(base: &str, href: &str) -> String {
    let href = href.split('#').next().unwrap_or("");
    let joined = if base.is_empty() {
        href.to_string()
    } else {
        format!("{base}/{href}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn open_zip(path: &Path) -> Result<zip::ZipArchive<std::fs::File>> {
    let f =
        std::fs::File::open(path).map_err(|e| DocError::corrupt(format!("EPUB 打不开: {e}")))?;
    zip::ZipArchive::new(f).map_err(|e| DocError::corrupt(format!("EPUB 打不开: {e}")))
}

fn read_entry(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Result<Option<String>> {
    match read_entry_bytes(zip, name)? {
        Some(bytes) => Ok(Some(decode_entry(&bytes))),
        None => Ok(None),
    }
}

/// 读原始字节（图片提取用）。
fn read_entry_bytes(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    match zip.by_name(name) {
        Ok(mut f) => {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)
                .map_err(|e| DocError::corrupt(format!("读取 {name} 失败: {e}")))?;
            Ok(Some(buf))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(DocError::corrupt(format!("ZIP 读取 {name} 失败: {e}"))),
    }
}

/// 条目解码：UTF-8（绝大多数）→ XML 声明嗅探的非 UTF-8（docforge 五件套，
/// feature `encoding` 下用 encoding_rs；未开则嗅探仅供参考）→
/// [`crate::encoding::decode`] 兜底——嗅探未命中时走统一解码链（BOM/UTF-16
/// / chardetng 概率探测 + GB18030/Big5 固定回退），EPUB 内非 UTF-8 且
/// 无声明的条目同样受益；全链未命中才 lossy。
fn decode_entry(bytes: &[u8]) -> String {
    if std::str::from_utf8(bytes).is_ok() {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    // XML 声明嗅探（先验：XML 自带的 encoding 声明比概率探测可信）：前 1KB
    // 内找 encoding="..."
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
                        break; // 声明 UTF-8 但字节非法：交给统一解码链兜底
                    }
                    let (s, _, had_err) = cs.decode(bytes);
                    if !had_err {
                        return s.into_owned();
                    }
                }
            }
        }
        let _ = enc; // 未开 encoding feature：嗅探结果仅供参考，走统一解码链
    }
    // 声明未命中 / 声明编码解码失败：统一解码链兜底（获得 chardetng +
    // GB18030/Big5 能力；EPUB 条目不会有 BOM 开头的合法 UTF-8——strict 已在
    // 入口命中，非 UTF-8 件由探测/固定链认领，最后才 lossy）
    crate::encoding::decode(bytes.to_vec())
}

fn year_of(date: &str) -> String {
    let bytes: Vec<char> = date.chars().collect();
    for w in bytes.windows(4) {
        if w.iter().all(|c| c.is_ascii_digit()) {
            return w.iter().collect();
        }
    }
    String::new()
}

// ── 分章 ────────────────────────────────────────────────────────────

/// 按标题切节：返回 (标题, 该节块下标)。
fn cut_indices(blocks: &[Block], max_level: u8) -> Vec<(String, Vec<usize>)> {
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    let mut title = String::new();
    let mut cur: Vec<usize> = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        if b.kind == BlockKind::Heading && b.level.unwrap_or(1) <= max_level {
            if !cur.is_empty() {
                out.push((std::mem::take(&mut title), std::mem::take(&mut cur)));
            }
            title = b.content.trim().to_string();
            continue;
        }
        if !b.content.is_empty() {
            cur.push(i);
        }
    }
    if !cur.is_empty() {
        out.push((title, cur));
    }
    out.retain(|(_, idx)| !idx.is_empty());
    out
}

/// 层级自适应切节：单节仍过大就试更深一层（h2 → h3 → h4）。
/// （writeback::epub 的页 provenance 复刻同源引用，保证切节口径一致。）
pub(crate) fn sections(blocks: &[Block]) -> Vec<(String, Vec<usize>)> {
    let mut best = Vec::new();
    for max_level in SECTION_LEVELS {
        best = cut_indices(blocks, max_level);
        let max_chars = best
            .iter()
            .map(|(_, idx)| {
                idx.iter()
                    .map(|&i| blocks[i].content.chars().count())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);
        if best.is_empty() || max_chars <= BIG_SECTION_CHARS {
            break;
        }
    }
    best
}

fn page_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter(|b| !b.content.is_empty())
        .map(|b| b.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn build_page(title: &str, blocks: Vec<Block>) -> Page {
    let text = page_text(&blocks);
    Page {
        name: if title.trim().is_empty() {
            None
        } else {
            Some(title.trim().to_string())
        },
        blocks,
        text,
        ..Default::default()
    }
}

// ── 能力 / 插件 ─────────────────────────────────────────────────────

/// 原始读取：把 spine 顺序的 XHTML 原文放进 `RawPage.items[0]`，书籍元数据进 `RawDoc.meta`。
#[derive(Default)]
pub struct EpubRawBackend {
    /// 提取 manifest 图片字节进首页 `images`（docforge 五件套，opt-in；
    /// M3 的图像 OCR 处理能力消费 `RawPage.images`）。
    pub ocr_images: bool,
}

impl ParseCapability for EpubRawBackend {
    fn id(&self) -> &str {
        "epub.read"
    }
    fn formats(&self) -> &[&str] {
        &["epub"]
    }
    fn parse(&self, path: &Path) -> Result<RawDoc> {
        let mut zip = open_zip(path)?;

        let container = read_entry(&mut zip, "META-INF/container.xml")?
            .ok_or_else(|| DocError::corrupt("EPUB 缺 META-INF/container.xml"))?;
        let opf_path = find_rootfile(&container)?
            .ok_or_else(|| DocError::corrupt("container.xml 缺 rootfile"))?;
        let opf_xml = read_entry(&mut zip, &opf_path)?
            .ok_or_else(|| DocError::corrupt(format!("EPUB 缺 OPF: {opf_path}")))?;
        let opf = parse_opf(&opf_xml)?;
        let opf_dir = opf_path
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();

        // 章节标题：EPUB3 nav 优先，其次 NCX
        let mut titles = BTreeMap::new();
        if let Some(nav) = opf
            .items
            .iter()
            .find(|it| it.props.split_whitespace().any(|p| p == "nav"))
            .map(|it| resolve(&opf_dir, &it.href))
        {
            if let Some(xml) = read_entry(&mut zip, &nav)? {
                titles = nav_titles(&xml);
            }
        }
        if titles.is_empty() {
            let ncx_path = opf
                .toc
                .as_ref()
                .and_then(|id| {
                    opf.items
                        .iter()
                        .find(|it| &it.id == id)
                        .map(|it| resolve(&opf_dir, &it.href))
                })
                .or_else(|| {
                    opf.items
                        .iter()
                        .find(|it| it.href.ends_with(".ncx"))
                        .map(|it| resolve(&opf_dir, &it.href))
                });
            if let Some(p) = ncx_path {
                if let Some(xml) = read_entry(&mut zip, &p)? {
                    titles = ncx_titles(&xml);
                }
            }
        }

        // 封面三级 fallback（docforge 五件套）：properties="cover-image" →
        // <meta name="cover"> → href 含 "cover" 的首个图片项
        let is_image = |it: &Item| it.media_type.starts_with("image/");
        let cover = opf
            .items
            .iter()
            .find(|it| it.props.split_whitespace().any(|p| p == "cover-image"))
            .map(|it| resolve(&opf_dir, &it.href))
            .or_else(|| {
                opf.cover_id.as_ref().and_then(|id| {
                    opf.items
                        .iter()
                        .find(|it| &it.id == id)
                        .map(|it| resolve(&opf_dir, &it.href))
                })
            })
            .or_else(|| {
                // B 的第三级：manifest 里 href 文件名含 "cover" 的图片
                opf.items
                    .iter()
                    .filter(|it| is_image(it) && it.href.to_lowercase().contains("cover"))
                    .map(|it| resolve(&opf_dir, &it.href))
                    .next()
            });

        let mut pages = Vec::new();
        for sr in &opf.spine {
            if !sr.linear {
                continue; // docforge 五件套：linear="no" 不进阅读流
            }
            let Some(item) = opf.items.iter().find(|it| it.id == sr.idref) else {
                continue;
            };
            let full = resolve(&opf_dir, &item.href);
            if full.ends_with(".ncx") {
                continue;
            }
            let Some(xhtml) = read_entry(&mut zip, &full)? else {
                continue;
            };
            let title = titles
                .get(&item.href)
                .or_else(|| titles.get(&full))
                .cloned()
                .unwrap_or_default();
            pages.push(RawPage {
                name: if title.is_empty() { None } else { Some(title) },
                items: vec![RawTextItem {
                    text: xhtml,
                    ..Default::default()
                }],
                ..Default::default()
            });
        }

        // docforge 五件套：图片字节提取（opt-in）。文档级资源放首页 images
        //（EPUB 无物理页概念；首页 images 通常为空，无信息损失）。
        if self.ocr_images {
            let mut images = Vec::new();
            for it in &opf.items {
                if !is_image(it) {
                    continue;
                }
                let full = resolve(&opf_dir, &it.href);
                if let Some(bytes) = read_entry_bytes(&mut zip, &full)? {
                    let ext = it
                        .href
                        .rsplit_once('.')
                        .map(|(_, e)| e.to_ascii_lowercase())
                        .unwrap_or_default();
                    images.push(crate::plugin::RawImage {
                        data: bytes,
                        ext,
                        ..Default::default()
                    });
                }
            }
            if let Some(first) = pages.first_mut() {
                first.images = images;
            }
        }

        let mut meta: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if !opf.title.is_empty() {
            meta.insert("epub.title".to_string(), opf.title.clone());
        }
        if !opf.authors.is_empty() {
            meta.insert("epub.authors".to_string(), opf.authors.join("; "));
        }
        if !opf.language.is_empty() {
            meta.insert("epub.language".to_string(), opf.language.clone());
        }
        if !opf.publisher.is_empty() {
            meta.insert("epub.publisher".to_string(), opf.publisher.clone());
        }
        let year = year_of(&opf.date);
        if !year.is_empty() {
            meta.insert("epub.year".to_string(), year);
        }
        if let Some(c) = &cover {
            meta.insert("epub.cover_href".to_string(), c.clone());
        }

        Ok(RawDoc {
            pages,
            meta,
            needs_ocr: false,
        })
    }
}

/// EPUB 插件：XHTML → 结构化块 → 按标题分章。
pub struct EpubPlugin;

impl FormatPlugin for EpubPlugin {
    fn id(&self) -> &str {
        "epub"
    }
    fn formats(&self) -> &[&str] {
        &["epub"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["epub.read"]
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
        // Document 口径与 Python 一致：单节 chapter 坍缩成一整段文本
        self.assemble_impl(raw, true)
    }
}

impl EpubPlugin {
    /// `collapse_single`：单节 chapter 是否坍缩成一段纯文本。
    ///
    /// - `true`（`FormatPlugin::assemble`，即 Document/CLI 输出）：与 Python
    ///   `read_epub` 的 `_xhtml_text` 回退一致，标题行并入正文；
    /// - `false`（`read_epub`，Rust 独有的结构化书 API）：保留 Heading/List 等块，
    ///   供下游做分章/排版使用。
    fn assemble_impl(&self, raw: &RawDoc, collapse_single: bool) -> Document {
        let mut pages: Vec<Page> = Vec::new();

        for (idx, rp) in raw.pages.iter().enumerate() {
            let Some(xhtml) = rp.items.first().map(|i| i.text.as_str()) else {
                continue;
            };
            let blocks = parse_blocks(xhtml);
            if blocks.iter().all(|b| b.content.is_empty()) {
                continue; // 纯图/空白页（封面页等）不成为章
            }

            let secs = sections(&blocks);
            if secs.len() > 1 {
                for (title, idxs) in secs {
                    let sec_blocks: Vec<Block> = idxs.iter().map(|&i| blocks[i].clone()).collect();
                    pages.push(build_page(&title, sec_blocks));
                }
                continue;
            }

            // 单节：Python `read_epub` 走 `_xhtml_text` 回退——整章坍缩成一段
            // 纯文本（标题行也并入正文，不再拆成 Heading），页名仍是 toc →
            // 文档内首个标题 → 第 N 章
            let first_heading = blocks
                .iter()
                .find(|b| b.kind == BlockKind::Heading && !b.content.trim().is_empty())
                .map(|b| b.content.trim().to_string());
            let title = rp
                .name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or(first_heading)
                .unwrap_or_else(|| format!("第{}章", idx + 1));
            if collapse_single {
                let text = page_text(&blocks);
                pages.push(Page {
                    name: if title.trim().is_empty() {
                        None
                    } else {
                        Some(title.trim().to_string())
                    },
                    blocks: vec![Block {
                        kind: BlockKind::Paragraph,
                        content: text.clone(),
                        ..Default::default()
                    }],
                    text,
                    ..Default::default()
                });
            } else {
                pages.push(build_page(&title, blocks));
            }
        }

        let mut doc = Document {
            pages,
            ..Default::default()
        };
        for key in [
            "epub.title",
            "epub.authors",
            "epub.language",
            "epub.publisher",
            "epub.year",
            "epub.cover_href",
        ] {
            if let Some(v) = raw.meta.get(key) {
                doc.meta.extra.insert(key.to_string(), v.clone());
            }
        }
        doc
    }
}

/// 注册 EPUB 后端与插件。
pub fn register_defaults(reg: &mut Registry) {
    let _ = reg.register_parse(Arc::new(EpubRawBackend::default()), None);
    reg.register_plugin(Arc::new(EpubPlugin));
}

// ── 便捷 API ────────────────────────────────────────────────────────

/// 一次读完：元数据 + 分章（含结构化块）。`path` 吃任何 `AsRef<Path>`。
pub fn read_epub(path: impl AsRef<Path>) -> Result<EpubBook> {
    let raw = EpubRawBackend::default().parse(path.as_ref())?;
    // 书 API 保留结构化块（Heading/List），不按 Document 口径坍缩
    let doc = EpubPlugin.assemble_impl(&raw, false);
    let get = |k: &str| raw.meta.get(k).cloned().unwrap_or_default();
    let meta = EpubMeta {
        title: get("epub.title"),
        authors: raw
            .meta
            .get("epub.authors")
            .map(|a| a.split("; ").map(|s| s.to_string()).collect())
            .unwrap_or_default(),
        language: get("epub.language"),
        publisher: get("epub.publisher"),
        year: get("epub.year"),
        cover_href: raw.meta.get("epub.cover_href").cloned(),
    };
    let chapters = doc
        .pages
        .iter()
        .map(|p| EpubChapter {
            title: p.name.clone().unwrap_or_default(),
            blocks: p.blocks.clone(),
            text: p.text.clone(),
        })
        .collect();
    Ok(EpubBook { meta, chapters })
}

/// 只取封面路径与书名（不解析正文），供书库/缩略图场景使用。
/// `path` 吃任何 `AsRef<Path>`。
pub fn epub_cover_href(path: impl AsRef<Path>) -> (Option<String>, String) {
    match EpubRawBackend::default().parse(path.as_ref()) {
        Ok(raw) => (
            raw.meta.get("epub.cover_href").cloned(),
            raw.meta.get("epub.title").cloned().unwrap_or_default(),
        ),
        Err(_) => (None, String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

    const OPF: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>测试书</dc:title>
    <dc:creator>作者甲</dc:creator>
    <dc:creator>作者乙</dc:creator>
    <dc:language>zh</dc:language>
    <dc:publisher>某社</dc:publisher>
    <dc:date>2021-05-01</dc:date>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ch1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="text/ch2.xhtml" media-type="application/xhtml+xml"/>
    <item id="cover" href="img/cover.jpg" media-type="image/jpeg" properties="cover-image"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
  </spine>
</package>"#;

    const NAV: &str = r#"<?xml version="1.0"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>
<nav epub:type="toc" xmlns:epub="http://www.idpf.org/2007/ops">
  <ol>
    <li><a href="text/ch1.xhtml">第一章</a></li>
    <li><a href="text/ch2.xhtml#sec1">第二章</a></li>
  </ol>
</nav></body></html>"#;

    const CH1: &str = r#"<html><body><h1>第一章</h1><p>正文一</p></body></html>"#;
    /// 单文档内两个 h2：应切成两章。
    const CH2: &str = r#"<html><body><h2>甲节</h2><p>甲正文</p><h2>乙节</h2><ul><li>要点</li></ul></body></html>"#;

    fn write_epub(dir_name: &str, title_opts: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("docforge_test")
            .join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("book.epub");
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        let mut put = |name: &str, body: &str| {
            zw.start_file(name, opts).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        };
        put("mimetype", "application/epub+zip");
        put("META-INF/container.xml", CONTAINER);
        put("OEBPS/content.opf", title_opts.unwrap_or(OPF));
        put("OEBPS/nav.xhtml", NAV);
        put("OEBPS/text/ch1.xhtml", CH1);
        put("OEBPS/text/ch2.xhtml", CH2);
        zw.finish().unwrap();
        path
    }

    #[test]
    fn reads_metadata_and_split_chapters() {
        let p = write_epub("ok", None);
        let book = read_epub(&p).unwrap();
        std::fs::remove_file(&p).ok();

        assert_eq!(book.meta.title, "测试书");
        assert_eq!(book.meta.authors, vec!["作者甲", "作者乙"]);
        assert_eq!(book.meta.language, "zh");
        assert_eq!(book.meta.publisher, "某社");
        assert_eq!(book.meta.year, "2021");
        assert_eq!(book.meta.cover_href.as_deref(), Some("OEBPS/img/cover.jpg"));

        // ch1 单节（保留 h1 块），ch2 按 h2 切成两节
        let titles: Vec<&str> = book.chapters.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["第一章", "甲节", "乙节"]);
        assert_eq!(book.chapters[0].blocks[0].kind, BlockKind::Heading);
        assert_eq!(book.chapters[1].text, "甲正文");
        assert_eq!(book.chapters[2].blocks[0].kind, BlockKind::List);
    }

    /// Document 口径与 Python 一致：单节 chapter 坍缩成一段文本（标题行并入正文）；
    /// 书 API `read_epub` 仍保留结构化块。
    #[test]
    fn single_section_chapter_collapses_in_document() {
        let p = write_epub("collapse", None);
        let mut reg = Registry::new();
        crate::plugins::text::register_defaults(&mut reg);
        register_defaults(&mut reg);
        let doc = crate::DocumentEngine::new(reg)
            .parse(&p)
            .unwrap();
        std::fs::remove_file(&p).ok();

        let page = &doc.pages[0]; // CH1：<h1>第一章</h1><p>正文一</p>
        assert_eq!(page.name.as_deref(), Some("第一章"));
        assert_eq!(page.blocks.len(), 1);
        assert_eq!(page.blocks[0].kind, BlockKind::Paragraph);
        assert_eq!(page.blocks[0].content, "第一章\n\n正文一");
        assert_eq!(page.text, "第一章\n\n正文一");
    }

    #[test]
    fn engine_route_and_meta_extra() {
        let p = write_epub("engine", None);
        let mut reg = Registry::new();
        crate::plugins::text::register_defaults(&mut reg);
        register_defaults(&mut reg);
        let doc = crate::DocumentEngine::new(reg)
            .parse(&p)
            .unwrap();
        std::fs::remove_file(&p).ok();

        assert_eq!(doc.meta.file_type, "epub");
        assert_eq!(doc.meta.page_count, 3);
        assert_eq!(
            doc.meta.extra.get("epub.title").map(|s| s.as_str()),
            Some("测试书")
        );
        assert!(doc.pages.iter().all(|p| p.name.is_some()));
    }

    #[test]
    fn broken_container_is_corrupt() {
        let dir = std::env::temp_dir()
            .join("docforge_test")
            .join("broken");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.epub");
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("mimetype", opts).unwrap();
        zw.write_all(b"application/epub+zip").unwrap();
        zw.finish().unwrap();

        let err = read_epub(&path).unwrap_err();
        std::fs::remove_file(&path).ok();
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(err.message.contains("container.xml"));
    }

    #[test]
    fn non_zip_is_corrupt() {
        let dir = std::env::temp_dir()
            .join("docforge_test")
            .join("notzip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.epub");
        std::fs::write(&path, b"not a zip").unwrap();
        let err = read_epub(&path).unwrap_err();
        std::fs::remove_file(&path).ok();
        assert_eq!(err.code.as_str(), "corrupt");
    }

    #[test]
    fn resolve_normalizes_dot_segments() {
        assert_eq!(resolve("OEBPS", "text/ch1.xhtml"), "OEBPS/text/ch1.xhtml");
        assert_eq!(resolve("OEBPS/text", "../img/a.png"), "OEBPS/img/a.png");
        assert_eq!(resolve("", "ch1.xhtml#frag"), "ch1.xhtml");
    }

    #[test]
    fn ncx_titles_pair_label_and_src() {
        let ncx = r#"<ncx><navMap>
            <navPoint id="n1"><navLabel><text>卷首</text></navLabel><content src="ch1.xhtml#top"/></navPoint>
            <navPoint id="n2"><navLabel><text>卷尾</text></navLabel><content src="ch2.xhtml"/></navPoint>
        </navMap></ncx>"#;
        let m = ncx_titles(ncx);
        assert_eq!(m.get("ch1.xhtml").map(|s| s.as_str()), Some("卷首"));
        assert_eq!(m.get("ch2.xhtml").map(|s| s.as_str()), Some("卷尾"));
    }
}

// ---------------------------------------------------------------------------
// docforge 五件套测试（M2 收尾）：linear=no / 封面第三级 / 图片提取 / 编码嗅探
// ---------------------------------------------------------------------------

#[cfg(test)]
mod five_piece_tests {
    use super::*;
    use std::io::Write;

    /// 最小 EPUB（可指定 spine 与 manifest 追加项）。
    fn build_epub(
        dir_name: &str,
        spine_xml: &str,
        manifest_extra: &str,
        extra_files: &[(&str, &[u8])],
    ) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("epub5-{dir_name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("book.epub");
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        zw.start_file("META-INF/container.xml", zip::write::SimpleFileOptions::default()).unwrap();
        zw.write_all(
            br#"<container><rootfiles><rootfile full-path="content.opf" media-type="a"/></rootfiles></container>"#,
        )
        .unwrap();
        let opf = format!(
            r#"<package xmlns="http://www.idpf.org/2007/opf">
<manifest>
<item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/>
{manifest_extra}
</manifest>
<spine>{spine_xml}</spine>
</package>"#
        );
        zw.start_file("content.opf", zip::write::SimpleFileOptions::default()).unwrap();
        zw.write_all(opf.as_bytes()).unwrap();
        zw.start_file("c1.xhtml", zip::write::SimpleFileOptions::default()).unwrap();
        zw.write_all(b"<html><body><p>chapter one</p></body></html>").unwrap();
        for (name, data) in extra_files {
            zw.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
        path
    }

    #[test]
    fn nonlinear_spine_items_skipped() {
        let p = build_epub(
            "linear",
            r#"<itemref idref="c1"/><itemref idref="nx" linear="no"/>"#,
            r#"<item id="nx" href="nx.xhtml" media-type="application/xhtml+xml"/>"#,
            &[("nx.xhtml", b"<html><body><p>hidden</p></body></html>")],
        );
        let raw = EpubRawBackend::default().parse(&p).unwrap();
        assert_eq!(raw.pages.len(), 1, "linear=no 不进阅读流");
        assert!(raw.pages[0].items[0].text.contains("chapter one"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn cover_fallback_to_filename() {
        // 无 cover-image 属性、无 meta cover，但文件名含 cover → 第三级命中
        let p = build_epub(
            "cover3",
            r#"<itemref idref="c1"/>"#,
            r#"<item id="cv" href="my-cover-image.png" media-type="image/png"/>"#,
            &[],
        );
        let (href, _) = epub_cover_href(&p);
        assert_eq!(href.as_deref(), Some("my-cover-image.png"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn ocr_images_extracts_manifest_images() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]; // PNG 头即可
        let p = build_epub(
            "imgs",
            r#"<itemref idref="c1"/>"#,
            r#"<item id="i1" href="pic.png" media-type="image/png"/>"#,
            &[("pic.png", &png)],
        );
        let raw = EpubRawBackend { ocr_images: true }
            .parse(&p)
            .unwrap();
        assert_eq!(raw.pages[0].images.len(), 1);
        assert_eq!(raw.pages[0].images[0].ext, "png");
        assert_eq!(&raw.pages[0].images[0].data[..4], &png[..4]);
        // 默认关：不提取
        let raw_off = EpubRawBackend::default().parse(&p).unwrap();
        assert!(raw_off.pages[0].images.is_empty());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn gbk_xhtml_decoded_via_declaration() {
        // 非 UTF-8 的 GBK XHTML，带 XML 声明 encoding="gbk"
        let gbk_bytes: Vec<u8> = vec![0xD6, 0xD0, 0xCE, 0xC4]; // "中文"
        let mut xhtml = br#"<?xml version="1.0" encoding="gbk"?><html><body><p>"#.to_vec();
        xhtml.extend_from_slice(&gbk_bytes);
        xhtml.extend_from_slice(b"</p></body></html>");
        let p = build_epub(
            "gbk",
            r#"<itemref idref="c1"/><itemref idref="g"/>"#,
            r#"<item id="g" href="gbk.xhtml" media-type="application/xhtml+xml"/>"#,
            &[("gbk.xhtml", &xhtml)],
        );
        let raw = EpubRawBackend::default().parse(&p).unwrap();
        assert_eq!(raw.pages.len(), 2);
        let got = &raw.pages[1].items[0].text;
        #[cfg(feature = "encoding")]
        assert!(got.contains("中文"), "GBK 应解码：{got:?}");
        #[cfg(not(feature = "encoding"))]
        assert!(!got.contains("中文"), "未开 encoding feature 应保持 lossy");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
