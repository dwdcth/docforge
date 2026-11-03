// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! HTML 插件：手写标签扫描（零第三方依赖），对齐 Python `html.parser` 的块级语义。
//!
//! 规则（与 Python `_HTMLCollector` 一致）：
//! * `h1..h6` → `Heading(level)`；
//! * `p` / `div` / `br` → `Paragraph`；
//! * `blockquote` → `Quote`；`li` → `List`（无序，不区分 ol/ul）；
//! * `pre` → `Code`；`table/tr/td/th` → `Table`；
//! * `script` / `style` / `noscript` / `svg` / `head` / `title` 整段跳过。
//!
//! 文本只做去首尾空白（不合并内部空白），与 Python 参考实现保持逐字可比。

use crate::model::{Block, BlockKind, Document, Page, TableData};
use crate::plugin::{FormatPlugin, RawDoc};

/// 整段跳过的标签（writeback::epub 的回写扫描同口径引用）。
pub(crate) const SKIP_TAGS: [&str; 6] = ["script", "style", "noscript", "svg", "head", "title"];

/// `h1..h6` → 1..6（writeback::epub 同口径引用）。
pub(crate) fn heading_level(tag: &str) -> Option<u8> {
    match tag {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "copy" => '©',
        "reg" => '®',
        "times" => '×',
        "middot" => '·',
        "ldquo" => '“',
        "rdquo" => '”',
        "lsquo" => '‘',
        "rsquo" => '’',
        _ => return None,
    })
}

fn numeric_entity(body: &str) -> Option<char> {
    let rest = body.strip_prefix('#')?;
    let code = match rest.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => rest.parse::<u32>().ok()?,
    };
    char::from_u32(code)
}

/// 解实体：`&amp;` 等命名实体与 `&#NN;` / `&#xHH;`；未知实体原样保留。
/// （writeback::epub 的回写扫描同口径引用。）
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let rest = &s[i..];
        let end = rest.find(';').filter(|e| *e <= 10);
        let decoded = end.and_then(|e| {
            let body = &rest[1..e];
            named_entity(body).or_else(|| numeric_entity(body))
        });
        match (end, decoded) {
            (Some(e), Some(ch)) => {
                out.push(ch);
                let skip_to = i + e + 1;
                while let Some((j, _)) = iter.peek() {
                    if *j < skip_to {
                        iter.next();
                    } else {
                        break;
                    }
                }
            }
            _ => out.push('&'),
        }
    }
    out
}

/// 引号感知的标签结束查找：属性值内的 `>` 不算标签边界（docforge 修补 ①）。
/// （writeback::epub 的回写扫描同口径引用。）
pub(crate) fn find_tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some(i),
                _ => {}
            },
        }
    }
    None
}

/// 从原始标签文本提取属性值（`alt="x"` / `alt='x'`；无引号单 token）。
fn extract_attr(raw: &str, name: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    let key = format!("{name}=");
    let pos = lower.find(&key)?;
    let rest = raw[pos + key.len()..].trim_start();
    // 带引号：取到闭合引号（值可含空格）；无引号：取到空白/标签边界
    let (v, end_ch): (&str, &[char]) = if let Some(v) = rest.strip_prefix('"') {
        (v, &['"'])
    } else if let Some(v) = rest.strip_prefix('\'') {
        (v, &['\''])
    } else {
        (rest, &[' ', '\t', '>'])
    };
    let end = v.find(end_ch).unwrap_or(v.len());
    let v = v[..end].trim();
    if v.is_empty() {
        None
    } else {
        Some(decode_entities(v))
    }
}

/// 标签名（小写化；writeback::epub 同口径引用）。
pub(crate) fn tag_name(raw: &str) -> String {
    raw.split(|c: char| c.is_whitespace() || c == '/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[derive(Default)]
struct Collector {
    blocks: Vec<Block>,
    buf: String,
    cell: String,
    row: Option<Vec<String>>,
    table: Option<Vec<Vec<String>>>,
    heading: Option<u8>,
    skip_depth: usize,
    /// 列表嵌套栈（docforge 修补 ③）：ol→true / ul→false，li 的 ordered 取栈顶
    list_stack: Vec<bool>,
}

impl Collector {
    fn flush(&mut self, kind: BlockKind) {
        let text = self.buf.trim().to_string();
        self.buf.clear();
        if text.is_empty() {
            return;
        }
        let mut block = Block {
            kind,
            content: text,
            ..Default::default()
        };
        if kind == BlockKind::Heading {
            block.level = self.heading.or(Some(1));
        }
        // 与 Python `_flush_text(BlockKind.LIST, ordered=False)` 一致：
        // `li` 不区分 ol/ul，一律按无序列表标记
        if kind == BlockKind::List {
            block.ordered = Some(self.list_stack.last().copied().unwrap_or(false));
        }
        self.blocks.push(block);
    }

    fn start_tag(&mut self, name: &str) {
        if SKIP_TAGS.contains(&name) {
            self.skip_depth += 1;
            return;
        }
        if self.skip_depth > 0 {
            return;
        }
        if let Some(level) = heading_level(name) {
            self.flush(BlockKind::Paragraph);
            self.heading = Some(level);
            return;
        }
        match name {
            "ol" => self.list_stack.push(true),
            "ul" => self.list_stack.push(false),
            "p" | "div" | "br" | "li" | "blockquote" => self.flush(BlockKind::Paragraph),
            "pre" => {
                self.flush(BlockKind::Paragraph);
                // pre 内空白按原文保留：这里不清 buf，只在 flush(Code) 时去首尾
            }
            "table" => {
                self.flush(BlockKind::Paragraph);
                self.table = Some(Vec::new());
            }
            "tr" => {
                if self.table.is_some() {
                    self.row = Some(Vec::new());
                }
            }
            "td" | "th" => self.cell.clear(),
            _ => {}
        }
    }

    fn end_tag(&mut self, name: &str) {
        if SKIP_TAGS.contains(&name) {
            self.skip_depth = self.skip_depth.saturating_sub(1);
            return;
        }
        if self.skip_depth > 0 {
            return;
        }
        if heading_level(name).is_some() {
            self.flush(BlockKind::Heading);
            self.heading = None;
            return;
        }
        match name {
            "ol" | "ul" => {
                self.list_stack.pop();
            }
            "pre" => self.flush(BlockKind::Code),
            "li" => self.flush(BlockKind::List),
            "td" | "th" => {
                let cell = self.cell.trim().to_string();
                if let Some(row) = self.row.as_mut() {
                    row.push(cell);
                }
                self.cell.clear();
            }
            "tr" => {
                if let (Some(table), Some(row)) = (self.table.as_mut(), self.row.take()) {
                    table.push(row);
                }
            }
            "table" => {
                if let Some(rows) = self.table.take() {
                    if !rows.is_empty() {
                        let content = rows[0].join(" | ");
                        self.blocks.push(Block {
                            kind: BlockKind::Table,
                            content,
                            table: Some(TableData {
                                rows,
                                ..Default::default()
                            }),
                            ..Default::default()
                        });
                    }
                }
            }
            "p" | "div" => self.flush(BlockKind::Paragraph),
            "blockquote" => self.flush(BlockKind::Quote),
            _ => {}
        }
    }

    /// img（void 元素）：直接产 Image 块（docforge 修补 ②）。
    /// `content` 语义是 OCR 文本故保持空；alt 进 `meta["alt"]` 供检索。
    fn on_img(&mut self, alt: Option<String>, src: Option<String>) {
        self.flush(BlockKind::Paragraph);
        let mut meta = std::collections::BTreeMap::new();
        if let Some(a) = alt {
            meta.insert("alt".to_string(), crate::model::MetaValue::Str(a));
        }
        // 资源定位（docforge 五件套的块级形态：EPUB 图片经 src 与 manifest 关联）
        if let Some(s) = src {
            meta.insert("src".to_string(), crate::model::MetaValue::Str(s));
        }
        self.blocks.push(Block {
            kind: BlockKind::Image,
            meta,
            ..Default::default()
        });
    }

    fn on_text(&mut self, data: &str) {
        if self.skip_depth > 0 || data.is_empty() {
            return;
        }
        let decoded = decode_entities(data);
        if self.row.is_some() {
            self.cell.push_str(&decoded); // 表格内：文本只进单元格
        } else {
            self.buf.push_str(&decoded);
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush(BlockKind::Paragraph);
        self.blocks
    }
}

/// 去注释（writeback::epub 的无块回退同口径引用）。
pub(crate) fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 解析 HTML 为块序列（畸形输入尽量降级，不 panic）。
pub fn parse_blocks(text: &str) -> Vec<Block> {
    let mut collector = Collector::default();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut text_start = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if text_start < i {
            collector.on_text(&text[text_start..i]);
        }
        // 注释整段丢弃
        if text[i..].starts_with("<!--") {
            i = match text[i + 4..].find("-->") {
                Some(rel) => i + 4 + rel + 3,
                None => bytes.len(),
            };
            text_start = i;
            continue;
        }
        match find_tag_end(&text[i..]) {
            Some(rel) => {
                let raw = &text[i + 1..i + rel];
                let trimmed = raw.trim();
                if let Some(name) = trimmed.strip_prefix('/') {
                    collector.end_tag(&tag_name(name));
                } else if !trimmed.is_empty()
                    && !trimmed.starts_with('!')
                    && !trimmed.starts_with('?')
                {
                    let name = tag_name(trimmed);
                    if name == "img" {
                        // img 是 void 元素：不进开闭栈，直接产块（修补 ②）
                        collector.on_img(extract_attr(trimmed, "alt"), extract_attr(trimmed, "src"));
                    } else {
                        collector.start_tag(&name);
                    }
                }
                i = i + rel + 1;
            }
            None => i = bytes.len(),
        }
        text_start = i;
    }
    if text_start < bytes.len() {
        collector.on_text(&text[text_start..]);
    }

    let blocks = collector.finish();
    if blocks.is_empty() {
        // 无标签或全是被跳过的内容：退化为一段归一化纯文本（注释不算正文）
        let plain = strip_comments(text)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if plain.is_empty() {
            return vec![];
        }
        return vec![Block {
            kind: BlockKind::Paragraph,
            content: plain,
            ..Default::default()
        }];
    }
    blocks
}

/// html 插件：整页一组块。
pub struct HtmlPlugin;

impl FormatPlugin for HtmlPlugin {
    fn id(&self) -> &str {
        "html"
    }
    fn formats(&self) -> &[&str] {
        &["html"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["html.parse"]
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        vec![]
    }
    fn assemble(&self, raw: &RawDoc) -> Document {
        let text = raw
            .pages
            .first()
            .and_then(|p| p.items.first())
            .map(|i| i.text.as_str())
            .unwrap_or_default();
        let blocks = parse_blocks(text);
        let page_text = blocks
            .iter()
            .filter(|b| !b.content.is_empty())
            .map(|b| b.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        Document {
            pages: vec![Page {
                blocks,
                text: page_text,
                ..Default::default()
            }],
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_paragraphs_and_lists() {
        let html = "<html><head><title>t</title></head><body>
            <h1>标题</h1><p>正文一</p><p>正文二</p>
            <ul><li>甲</li><li>乙</li></ul>
            <blockquote>引用</blockquote></body></html>";
        let blocks = parse_blocks(html);
        let kinds: Vec<BlockKind> = blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Heading,
                BlockKind::Paragraph,
                BlockKind::Paragraph,
                BlockKind::List,
                BlockKind::List,
                BlockKind::Quote
            ]
        );
        assert_eq!(blocks[0].content, "标题");
        assert_eq!(blocks[0].level, Some(1));
        assert_eq!(blocks[1].content, "正文一");
        assert_eq!(blocks[3].content, "甲");
    }

    #[test]
    fn table_rows_become_table_block() {
        let html = "<table><tr><th>h1</th><th>h2</th></tr><tr><td>1</td><td>2</td></tr></table>";
        let blocks = parse_blocks(html);
        assert_eq!(blocks.len(), 1);
        let rows = &blocks[0].table.as_ref().unwrap().rows;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["h1", "h2"]);
        assert_eq!(rows[1], vec!["1", "2"]);
        assert_eq!(blocks[0].content, "h1 | h2");
    }

    #[test]
    fn script_style_and_entities() {
        let html = "<style>p{color:red}</style><script>var a=1;</script><p>a &amp; b &lt;3 &nbsp;c &#65;</p>";
        let blocks = parse_blocks(html);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].content.contains("a & b <3"));
        assert!(blocks[0].content.ends_with("c A"));
    }

    #[test]
    fn pre_keeps_inner_newlines() {
        let html = "<pre>line1\nline2</pre>";
        let blocks = parse_blocks(html);
        assert_eq!(blocks[0].kind, BlockKind::Code);
        assert_eq!(blocks[0].content, "line1\nline2");
    }

    #[test]
    fn plain_text_fallback_and_comments() {
        assert_eq!(parse_blocks("no tags here").len(), 1);
        assert!(parse_blocks("<!-- only comment -->").is_empty());
        let html = "<body><!-- x --><p>visible</p></body>";
        assert_eq!(parse_blocks(html)[0].content, "visible");
    }

    #[test]
    fn malformed_html_does_not_panic() {
        let _ = parse_blocks("<p>unclosed <b>bold");
        let _ = parse_blocks("<");
        let _ = parse_blocks("<<>>");
    }
}

// ---------------------------------------------------------------------------
// docforge 三修补的测试（M2b）：引号内 >、img→Image、ol 有序
// ---------------------------------------------------------------------------

#[cfg(test)]
mod m2b_tests {
    use super::*;

    #[test]
    fn attribute_value_with_gt_not_truncated() {
        // 属性值里的 > 不能截断标签（修补 ①）
        let html = r#"<p title="a>b">正文</p><p>第二段</p>"#;
        let blocks = parse_blocks(html);
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert_eq!(blocks[0].content, "正文");
        assert_eq!(blocks[1].content, "第二段");
    }

    #[test]
    fn img_produces_image_block_with_alt() {
        let html = r#"<p>前文</p><img src="x.png" alt="山景 图"><p>后文</p>"#;
        let blocks = parse_blocks(html);
        let img = blocks.iter().find(|b| b.kind == BlockKind::Image).expect("应有 Image 块");
        assert_eq!(img.content, "");
        assert_eq!(
            img.meta.get("alt"),
            Some(&crate::model::MetaValue::Str("山景 图".to_string()))
        );
    }

    #[test]
    fn ol_list_items_ordered() {
        let html = r#"<ol><li>甲</li><li>乙</li></ol><ul><li>子</li></ul>"#;
        let blocks = parse_blocks(html);
        let lists: Vec<&Block> = blocks.iter().filter(|b| b.kind == BlockKind::List).collect();
        assert_eq!(lists.len(), 3);
        assert_eq!(lists[0].ordered, Some(true), "ol 内 li 有序");
        assert_eq!(lists[1].ordered, Some(true));
        assert_eq!(lists[2].ordered, Some(false), "ul 内 li 无序");
    }
}
