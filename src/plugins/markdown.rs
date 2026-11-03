// 前身为 docparser-core 0.2.1 吸收的手写块级解析（MIT OR Apache-2.0，见
// THIRD_PARTY_NOTICES.md）。本版以 pulldown-cmark 0.13 事件流重写（CommonMark +
// GFM 表格 / 删除线 / 任务列表），事件流→结构映射方式参考 vendor 的
// document-parser 0.1.0-alpha.18 formats/markdown/loader.rs（同见声明表）。
// frontmatter 剥离思路取自 ebook-rs 0.18（vendor 存档）；本版补上手写键值
// 解析，title/authors/language 经 FormatPlugin::enrich_meta 后置钩子进
// DocMeta.extra（原「trait 扩展」遗留项就此清账，见下方模块注释）。
//! Markdown → 块级 IR：pulldown-cmark 事件流直接映射，不渲染成 HTML 再反推。
//!
//! 覆盖 CommonMark + GFM（表格 / 删除线 / 任务列表）。映射规则（与旧手写版
//! 保持的产出契约，行为差异处**以 pulldown 为准**并在测试注明）：
//! * ATX `#{1,6}` 与 setext（`Title\n===`）→ `Heading(level)`，level 直传；
//! * 段落 → `Paragraph`，软换行以空格并入（对齐旧版连续行合并语义）；
//! * 整个列表产**一个** `List` 块，项以 `\n` 连接；嵌套项保守拍平为同层项
//!   （IR 无嵌套语义）；`ordered` 取最外层列表（起始编号 IR 无字段，丢弃）；
//! * 代码围栏 / 缩进块 → `Code`：围栏 info 串整段进 `lang`（对齐旧版取反引号
//!   后整段），缩进块无 `lang`；
//! * 引用 → **一个** `Quote` 块（旧版每 `>` 行一块，pulldown 语义下整段引用
//!   是一个块，内部行以 `\n` 分行）；
//! * GFM 表格 → `Table`：首行表头进 `content` 与 `rows[0]`，`header_rows`
//!   维持旧版不设（= 0，与 html.rs 同口径）；
//! * `---` Rule 忽略（旧版会误落成段落文本，属手写实现副产物）；
//! * 行内标记拍平为纯文本：链接保留文字、URL 丢弃；行内代码保留原文；
//!   任务列表 `[ ]` / `[x]` 前缀保留在项文本里（GFM 特性，不丢字符）；
//! * 图片：顶层段落内产 `Image` 块（alt/src 进 `meta`，对齐 html.rs 的
//!   on_img 形态）；列表 / 引用 / 表格内图片拍平为 alt 文本（URL 丢弃）。
//! * frontmatter（首行 `---` 且 50 行内闭合）解析前剥离，不进正文块与页文本；
//!   title/authors/language 经 `FormatPlugin::enrich_meta` 后置钩子写入
//!   `DocMeta.extra`（键 `md.title` / `md.authors` / `md.language`）。engine 在
//!   assemble 与 finalize 之间调钩子；原文仍完整留在 raw 里，钩子独立定位解析，
//!   与剥离阶段互不依赖、不重复处理正文。

use crate::model::{Block, BlockKind, Document, MetaValue, Page, TableData};
use crate::plugin::{FormatPlugin, RawDoc};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// frontmatter 定位：首行恰为 `---` 且前 50 行内出现闭合 `---` → 返回
/// `(元数据区切片, 正文起点字节偏移)`；否则 None（未闭合的 `---` 按 Rule /
/// setext 交给 pulldown 正常处理）。
fn frontmatter_span(text: &str) -> Option<(&str, usize)> {
    // 开头必须是一个独立的 `---` 行（容许 CRLF）
    let first_ok = text.starts_with("---")
        && text
            .as_bytes()
            .get(3)
            .map_or(text.len() == 3, |b| *b == b'\n' || *b == b'\r');
    if !first_ok {
        return None;
    }
    // 元数据区从首行换行符之后开始（恰为三个字节的 `"---"` 时为空区）
    let meta_start = 3
        + usize::from(text.as_bytes().get(3) == Some(&b'\r'))
        + usize::from(text.as_bytes().get(3) == Some(&b'\n'));
    let mut off = 0usize;
    for (idx, line) in text.lines().enumerate() {
        if idx >= 50 {
            return None; // 50 行内未闭合：不当 frontmatter
        }
        if idx > 0 && line.trim_end() == "---" {
            // 正文从闭合行的换行符之后开始
            let mut end = off + line.len();
            end += usize::from(text.as_bytes().get(end) == Some(&b'\r'));
            end += usize::from(text.as_bytes().get(end) == Some(&b'\n'));
            return Some((&text[meta_start..off], end));
        }
        // 行推进按实际行尾回填 `\n` / `\r\n`（固定 +1 会在 CRLF 文件上漂移，
        // 正文切片随之错位）
        off += line.len();
        if text.as_bytes().get(off) == Some(&b'\r') {
            off += 1;
        }
        if text.as_bytes().get(off) == Some(&b'\n') {
            off += 1;
        }
    }
    None
}

/// frontmatter 剥离：有闭合区则返回其后正文；否则原样返回。
fn strip_frontmatter(text: &str) -> &str {
    frontmatter_span(text).map_or(text, |(_, body)| &text[body..])
}

/// 去掉一层成对包裹引号（`"x"` / `'x'` → `x`；非包裹形态原样返回）。
fn unquote(value: &str) -> &str {
    for q in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(q) && value.ends_with(q) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// authors 值 → `"; "` 连接串（对齐 fb2 的 `fb2.authors` 口径）：
/// `张三, 李四` 逗号列表，或 `[a, b]` / `["a", "b"]` 方括号列表形态；
/// 条目各自去引号去空白，空条目丢弃。
fn authors_joined(value: &str) -> String {
    let v = value.trim();
    let inner = if v.len() >= 2 && v.starts_with('[') && v.ends_with(']') {
        &v[1..v.len() - 1]
    } else {
        v
    };
    inner
        .split(',')
        .map(|s| unquote(s.trim()).trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// frontmatter 键值提取（手写 `key: value`，不为此引 yaml 依赖）：
/// * 键大小写不敏感（归一为小写），键 / 值两侧空白容忍；
/// * 值去掉一层成对包裹引号；
/// * 无冒号的行跳过（注释 / 列表项等不在支持形态内）。
fn parse_frontmatter_kv(meta_region: &str) -> std::collections::BTreeMap<String, String> {
    let mut kv = std::collections::BTreeMap::new();
    for line in meta_region.lines() {
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        kv.insert(key, unquote(value.trim()).to_string());
    }
    kv
}

/// 顶层列表累积器：整列表一块，项以 `\n` 连接（对齐旧版逐 run 产出粒度）。
struct ListAcc {
    ordered: bool,
    items: Vec<String>,
    cur: String,
}

/// 表格累积器：rows[row][col] = 单元格文本。
#[derive(Default)]
struct TableAcc {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
}

/// pulldown-cmark 事件流 → 块序列的累积器。行内文本按「最内层开放结构」路由：
/// 表格单元格 > 代码块 > 标题 > 列表项 > 引用 > 段落。
#[derive(Default)]
struct Walker {
    blocks: Vec<Block>,
    para: String,
    heading: Option<(u8, String)>,
    code: Option<(Option<String>, String)>,
    quote: Option<String>,
    list: Option<ListAcc>,
    table: Option<TableAcc>,
    /// 嵌套列表深度：>0 表示 List 处于外层列表 / 引用内（项拍平进当前语境）
    nest: usize,
    /// 待产出图片（顶层段落内）：dest_url + alt 文本累积。
    /// 0.13 的 `Tag::Image` 不带 alt 字段，alt 来自 Start/End 之间的内联事件。
    img: Option<(String, String)>,
    /// 图片嵌套深度（`![a ![b](c)](d)`：内层文本并入外层 alt）
    img_depth: usize,
}

impl Walker {
    /// 是否处于顶层语境（顶层专属结构只在这里开启，嵌套一律拍平为文本）。
    fn top_level(&self) -> bool {
        self.heading.is_none()
            && self.code.is_none()
            && self.quote.is_none()
            && self.list.is_none()
            && self.table.is_none()
    }

    /// 行内文本路由到最内层开放结构。
    fn sink(&mut self, s: &str) {
        if let Some((_, alt)) = self.img.as_mut() {
            alt.push_str(s);
        } else if let Some(t) = self.table.as_mut() {
            t.cell.push_str(s);
        } else if let Some((_, buf)) = self.code.as_mut() {
            buf.push_str(s);
        } else if let Some((_, buf)) = self.heading.as_mut() {
            buf.push_str(s);
        } else if let Some(l) = self.list.as_mut() {
            l.cur.push_str(s);
        } else if let Some(q) = self.quote.as_mut() {
            q.push_str(s);
        } else {
            self.para.push_str(s);
        }
    }

    /// 换行并入：引用内保留行结构（旧行为是每行一块，合并成一块后以 `\n` 分行），
    /// 其余语境以空格并入（对齐旧版连续行合并语义）。
    fn break_inline(&mut self) {
        if self.quote.is_some() {
            self.sink("\n");
        } else {
            self.sink(" ");
        }
    }

    fn push_para(&mut self) {
        let content = std::mem::take(&mut self.para);
        let content = content.trim();
        if !content.is_empty() {
            self.blocks.push(Block {
                kind: BlockKind::Paragraph,
                content: content.to_string(),
                ..Default::default()
            });
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } if self.top_level() => {
                self.push_para();
                self.heading = Some((level as u8, String::new()));
            }
            Tag::Heading { .. } => {} // 引用 / 列表内标题拍平为文本
            Tag::Paragraph if self.top_level() => {} // 顶层段落：文本自由流入
            Tag::Paragraph => {
                // 列表项 / 引用内的段落不切块；多段之间补分隔，避免粘连
                if let Some(q) = self.quote.as_mut() {
                    if !q.trim().is_empty() {
                        q.push('\n');
                    }
                } else if let Some(l) = self.list.as_mut() {
                    if !l.cur.trim().is_empty() {
                        l.cur.push(' ');
                    }
                }
            }
            Tag::CodeBlock(kind) if self.top_level() => {
                self.push_para();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let info = info.trim();
                        (!info.is_empty()).then(|| info.to_string())
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::CodeBlock(_) => {} // 嵌套代码块拍平为文本
            Tag::List(start) => {
                if self.list.is_some() || !self.top_level() {
                    // 嵌套列表：IR 无嵌套语义，保守拍平为当前语境的同层项
                    self.nest += 1;
                } else {
                    self.push_para();
                    self.list = Some(ListAcc {
                        ordered: start.is_some(),
                        items: Vec::new(),
                        cur: String::new(),
                    });
                }
            }
            Tag::Item => {
                // 上一项收口、开新项（列表不在顶层语境时无 acc，文本直接流入）
                if let Some(l) = self.list.as_mut() {
                    let cur = std::mem::take(&mut l.cur);
                    let cur = cur.trim();
                    if !cur.is_empty() {
                        l.items.push(cur.to_string());
                    }
                }
            }
            Tag::BlockQuote(_) if self.top_level() => {
                self.push_para();
                self.quote = Some(String::new());
            }
            Tag::BlockQuote(_) => {} // 嵌套引用拍平
            Tag::Table(_) if self.top_level() => {
                self.push_para();
                self.table = Some(TableAcc::default());
            }
            Tag::Table(_) => {} // 嵌套表格拍平（单元格文本流入当前语境）
            // 行内标记：Start/End 均无副作用，纯文本自然流入
            // （表格体无独立 tag：TableHead 闭合后即数据行）
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough => {}
            Tag::Link { .. } => {} // 链接保留文字、URL 丢弃
            Tag::Superscript | Tag::Subscript => {}
            Tag::Image { dest_url, .. } => {
                if self.img.is_some() {
                    // 图片嵌图片：内层文本并入外层 alt，dest 丢弃
                    self.img_depth += 1;
                } else if self.top_level() {
                    // 顶层段落：先落已积累文本，alt 由后续内联事件累积
                    // （对齐 html.rs 的 on_img 形态：content 空、alt/src 进 meta）
                    self.push_para();
                    self.img = Some((dest_url.to_string(), String::new()));
                    self.img_depth = 1;
                }
                // 非顶层语境（列表 / 引用 / 表格）：拍平为 alt 文本，URL 丢弃
            }
            _ => {} // HtmlBlock / FootnoteDefinition 等：内部事件照常流入
        }
    }

    fn end_tag(&mut self, end: TagEnd) {
        match end {
            TagEnd::Heading(_) => {
                if let Some((level, buf)) = self.heading.take() {
                    let content = buf.trim();
                    if !content.is_empty() {
                        self.blocks.push(Block {
                            kind: BlockKind::Heading,
                            content: content.to_string(),
                            level: Some(level),
                            ..Default::default()
                        });
                    }
                }
            }
            TagEnd::Paragraph if self.top_level() => self.push_para(),
            TagEnd::CodeBlock => {
                if let Some((lang, buf)) = self.code.take() {
                    // 结尾换行不进 content（对齐旧版 buf.join("\n") 的口径）
                    self.blocks.push(Block {
                        kind: BlockKind::Code,
                        content: buf.trim_end_matches('\n').to_string(),
                        lang,
                        ..Default::default()
                    });
                }
            }
            TagEnd::BlockQuote(_) => {
                if let Some(q) = self.quote.take() {
                    let content = q.trim();
                    if !content.is_empty() {
                        self.blocks.push(Block {
                            kind: BlockKind::Quote,
                            content: content.to_string(),
                            ..Default::default()
                        });
                    }
                }
            }
            TagEnd::List(_) => {
                if self.nest > 0 {
                    self.nest -= 1;
                } else if let Some(mut l) = self.list.take() {
                    let cur = l.cur.trim();
                    if !cur.is_empty() {
                        l.items.push(cur.to_string());
                    }
                    if !l.items.is_empty() {
                        self.blocks.push(Block {
                            kind: BlockKind::List,
                            content: l.items.join("\n"),
                            ordered: Some(l.ordered),
                            ..Default::default()
                        });
                    }
                }
            }
            TagEnd::TableHead => {
                // 表头单元格不经 TableRow 包装（0.13 事件流形态），行末在此收口
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    let cell = std::mem::take(&mut t.cell);
                    t.row.push(cell.trim().to_string());
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    if let Some(header) = t.rows.first() {
                        let content = header.join(" | ");
                        self.blocks.push(Block {
                            kind: BlockKind::Table,
                            content,
                            table: Some(TableData {
                                rows: t.rows,
                                ..Default::default()
                            }),
                            ..Default::default()
                        });
                    }
                }
            }
            TagEnd::Image => {
                if self.img_depth > 0 {
                    self.img_depth -= 1;
                }
                if self.img_depth == 0 {
                    if let Some((src, alt)) = self.img.take() {
                        let mut meta = std::collections::BTreeMap::new();
                        let alt = alt.trim().to_string();
                        if !alt.is_empty() {
                            meta.insert("alt".to_string(), MetaValue::Str(alt));
                        }
                        if !src.is_empty() {
                            meta.insert("src".to_string(), MetaValue::Str(src));
                        }
                        self.blocks.push(Block {
                            kind: BlockKind::Image,
                            meta,
                            ..Default::default()
                        });
                    }
                }
            }
            _ => {}
        }
    }

    fn on_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(end) => self.end_tag(end),
            Event::Text(t) => self.sink(&t),
            Event::Code(c) => self.sink(&c), // 行内代码保留原文
            Event::SoftBreak | Event::HardBreak => self.break_inline(),
            Event::TaskListMarker(checked) => {
                // GFM 任务列表前缀保留在项文本里（IR 无 checkbox 字段，不丢字符）
                self.sink(if checked { "[x] " } else { "[ ] " });
            }
            Event::Rule => {} // --- 分隔线忽略（旧版误落成段落文本，不保留）
            Event::Html(h) | Event::InlineHtml(h) => self.sink(&h), // 原样并入文本
            Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {
                // 未启用对应 option（footnotes / math），防御性忽略
            }
        }
    }
}

/// 解析 Markdown 文本为块序列（frontmatter 在此统一剥离，正文块不含 `---`
/// 元数据区；剥出物的键值不经此函数——`MarkdownPlugin::enrich_meta` 从原文
/// 另行定位解析，写入 `DocMeta.extra`）。
pub fn parse(text: &str) -> Vec<Block> {
    let body = strip_frontmatter(text);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut walker = Walker::default();
    for event in Parser::new_ext(body, options) {
        walker.on_event(event);
    }
    walker.push_para(); // 收尾段落（无 End(Paragraph) 兜底的场景）
    walker.blocks
}

/// md 插件：原文交给 [`parse`]，整页一组块；页文本为剥离 frontmatter 后的正文。
pub struct MarkdownPlugin;

impl FormatPlugin for MarkdownPlugin {
    fn id(&self) -> &str {
        "md"
    }
    fn formats(&self) -> &[&str] {
        &["md"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["text.read.md"]
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
        let body = strip_frontmatter(text);
        Document {
            pages: vec![Page {
                blocks: parse(body),
                text: body.to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// 组装后置钩子：frontmatter 元数据 → `DocMeta.extra`。原文取自 **raw**
    /// （`assemble` 已把 frontmatter 从正文剥掉，而 `raw.pages[0].items[0].text`
    /// 仍是原文），这里独立定位解析——两阶段互不依赖、不重复剥离正文。
    /// frontmatter 不存在或未闭合时不产键；空值键同样不进 extra。
    fn enrich_meta(&self, raw: &RawDoc, doc: &mut Document) {
        let Some(text) = raw
            .pages
            .first()
            .and_then(|p| p.items.first())
            .map(|i| i.text.as_str())
        else {
            return;
        };
        let Some((meta_region, _)) = frontmatter_span(text) else {
            return;
        };
        let kv = parse_frontmatter_kv(meta_region);
        if let Some(title) = kv.get("title").map(String::as_str).filter(|s| !s.is_empty()) {
            doc.meta.extra.insert("md.title".to_string(), title.to_string());
        }
        if let Some(authors) = kv.get("authors").map(String::as_str) {
            let joined = authors_joined(authors);
            if !joined.is_empty() {
                doc.meta.extra.insert("md.authors".to_string(), joined);
            }
        }
        if let Some(lang) = kv.get("language").map(String::as_str).filter(|s| !s.is_empty()) {
            doc.meta
                .extra
                .insert("md.language".to_string(), lang.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(blocks: &[Block]) -> Vec<BlockKind> {
        blocks.iter().map(|b| b.kind).collect()
    }

    #[test]
    fn parses_headings_and_paragraphs() {
        let blocks = parse("# 标题\n正文一\n正文二\n\n## 二级\n");
        assert_eq!(
            kinds(&blocks),
            vec![BlockKind::Heading, BlockKind::Paragraph, BlockKind::Heading]
        );
        assert_eq!(blocks[0].content, "标题");
        assert_eq!(blocks[0].level, Some(1));
        assert_eq!(blocks[1].content, "正文一 正文二"); // 连续行合并为一段
        assert_eq!(blocks[2].level, Some(2));
    }

    #[test]
    fn parses_lists_and_quote() {
        let blocks = parse("- a\n- b\n\n1. x\n2. y\n\n> 引用\n");
        assert_eq!(
            kinds(&blocks),
            vec![BlockKind::List, BlockKind::List, BlockKind::Quote]
        );
        assert_eq!(blocks[0].content, "a\nb");
        assert_eq!(blocks[0].ordered, Some(false));
        assert_eq!(blocks[1].ordered, Some(true));
        assert_eq!(blocks[2].content, "引用");
    }

    #[test]
    fn parses_code_fence_with_lang() {
        let blocks = parse("```rust\nfn main() {}\n```\n");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Code);
        assert_eq!(blocks[0].lang.as_deref(), Some("rust"));
        assert_eq!(blocks[0].content, "fn main() {}");
    }

    #[test]
    fn parses_pipe_table() {
        let blocks = parse("| a | b |\n| --- | --- |\n| 1 | 2 |\n");
        assert_eq!(blocks.len(), 1);
        let rows = &blocks[0].table.as_ref().unwrap().rows;
        assert_eq!(rows.len(), 2); // 表头 + 1 数据行（分隔行不计入）
        assert_eq!(rows[0], vec!["a", "b"]);
        assert_eq!(rows[1], vec!["1", "2"]);
        assert_eq!(blocks[0].content, "a | b");
    }

    #[test]
    fn plain_pipe_line_without_separator_is_paragraph() {
        let blocks = parse("| a | b |\nnot a table\n");
        assert_eq!(kinds(&blocks), vec![BlockKind::Paragraph]);
    }

    #[test]
    fn unmatched_fence_does_not_hang() {
        let blocks = parse("```\nabc");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Code);
    }
}

// ---------------------------------------------------------------------------
// pulldown-cmark 重写后的新增测试（M6）：GFM 表格 / 任务列表 / setext 标题 /
// 围栏 info 串 / frontmatter 剥离 / 嵌套列表拍平 / 图片块 / 行内拍平
// ---------------------------------------------------------------------------

#[cfg(test)]
mod pulldown_tests {
    use super::*;

    fn kinds(blocks: &[Block]) -> Vec<BlockKind> {
        blocks.iter().map(|b| b.kind).collect()
    }

    /// GFM 对齐冒号行 + 单元格内删除线拍平。
    #[test]
    fn parses_gfm_table_with_alignment() {
        let blocks = parse("| 左 | 右 |\n| :--- | ---: |\n| ~~删~~ | 1 |\n");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Table);
        let rows = &blocks[0].table.as_ref().unwrap().rows;
        assert_eq!(rows[0], vec!["左", "右"]);
        assert_eq!(rows[1], vec!["删", "1"]); // 删除线拍平，字符保留
        assert_eq!(blocks[0].content, "左 | 右");
    }

    /// 任务列表 `[ ]` / `[x]` 前缀保留在项文本里（不丢字符）。
    #[test]
    fn task_list_markers_kept() {
        let blocks = parse("- [ ] 未办\n- [x] 已办\n");
        assert_eq!(kinds(&blocks), vec![BlockKind::List]);
        assert_eq!(blocks[0].content, "[ ] 未办\n[x] 已办");
        assert_eq!(blocks[0].ordered, Some(false));
    }

    /// setext 标题（`Title\n===` / `Sub\n---`）映射为 Heading。
    #[test]
    fn setext_heading_maps_to_heading() {
        let blocks = parse("Title\n===\n\nSub\n---\n");
        assert_eq!(
            kinds(&blocks),
            vec![BlockKind::Heading, BlockKind::Heading]
        );
        assert_eq!(blocks[0].content, "Title");
        assert_eq!(blocks[0].level, Some(1));
        assert_eq!(blocks[1].content, "Sub");
        assert_eq!(blocks[1].level, Some(2));
    }

    /// 围栏 info 串整段进 `lang`（对齐旧版取反引号后整段的口径）。
    #[test]
    fn code_fence_info_string_kept_as_lang() {
        let blocks = parse("```rust ignore\nfn a() {}\n```\n");
        assert_eq!(blocks[0].lang.as_deref(), Some("rust ignore"));
        // 缩进代码块无 lang
        let blocks = parse("\n    indented\n");
        assert_eq!(blocks[0].kind, BlockKind::Code);
        assert_eq!(blocks[0].lang, None);
        assert_eq!(blocks[0].content, "indented");
    }

    /// frontmatter 剥离：不进正文块。
    #[test]
    fn frontmatter_is_stripped_from_blocks() {
        let blocks = parse("---\ntitle: 书名\nauthors: 张三, 李四\nlanguage: zh\n---\n\n正文\n");
        assert_eq!(kinds(&blocks), vec![BlockKind::Paragraph]);
        assert_eq!(blocks[0].content, "正文");
    }

    /// 未闭合的 frontmatter 不误剥：`---` 按 Rule 忽略，其余文本保留为段落。
    #[test]
    fn frontmatter_without_closer_is_content() {
        let blocks = parse("---\ntitle: 书名\n没有闭合\n");
        assert_eq!(kinds(&blocks), vec![BlockKind::Paragraph]);
        assert_eq!(blocks[0].content, "title: 书名 没有闭合");
    }

    /// 嵌套列表保守拍平为同层项（IR 无嵌套语义），有序性取最外层。
    #[test]
    fn nested_lists_flatten_to_same_level() {
        let blocks = parse("- a\n  - b\n    - c\n- d\n");
        assert_eq!(kinds(&blocks), vec![BlockKind::List]);
        assert_eq!(blocks[0].content, "a\nb\nc\nd");
        assert_eq!(blocks[0].ordered, Some(false));

        let blocks = parse("1. x\n   1. y\n");
        assert_eq!(kinds(&blocks), vec![BlockKind::List]);
        assert_eq!(blocks[0].content, "x\ny");
        assert_eq!(blocks[0].ordered, Some(true));
    }

    /// 顶层段落内图片产 Image 块（alt/src 进 meta，对齐 html.rs 的 on_img）。
    #[test]
    fn paragraph_image_produces_image_block() {
        let blocks = parse("前文\n\n![山景 图](img/x.png)\n\n后文\n");
        assert_eq!(
            kinds(&blocks),
            vec![BlockKind::Paragraph, BlockKind::Image, BlockKind::Paragraph]
        );
        assert_eq!(blocks[1].content, "");
        assert_eq!(
            blocks[1].meta.get("alt"),
            Some(&MetaValue::Str("山景 图".to_string()))
        );
        assert_eq!(
            blocks[1].meta.get("src"),
            Some(&MetaValue::Str("img/x.png".to_string()))
        );
    }

    /// 行内标记拍平为纯文本：链接保留文字、URL 丢弃，行内代码 / 删除线保留字符。
    #[test]
    fn inline_markup_flattens_to_plain_text() {
        let blocks = parse("**粗** and [链接](https://a.b/c) and `码` and ~~删~~\n");
        assert_eq!(kinds(&blocks), vec![BlockKind::Paragraph]);
        assert_eq!(blocks[0].content, "粗 and 链接 and 码 and 删");
    }

    /// 行为差异注明：整段引用合并为一个 Quote 块（旧版每 `>` 行一块）。
    #[test]
    fn multiline_quote_is_single_block() {
        let blocks = parse("> 第一行\n> 第二行\n");
        assert_eq!(kinds(&blocks), vec![BlockKind::Quote]);
        assert_eq!(blocks[0].content, "第一行\n第二行");
    }

    /// `---` 分隔线忽略（旧版误落成段落文本）。
    #[test]
    fn thematic_rule_is_ignored() {
        let blocks = parse("前文\n\n---\n\n后文\n");
        assert_eq!(kinds(&blocks), vec![BlockKind::Paragraph, BlockKind::Paragraph]);
        assert_eq!(blocks[0].content, "前文");
        assert_eq!(blocks[1].content, "后文");
    }

    /// assemble 侧：frontmatter 不进页文本与块；插件契约（整页一组块）不变。
    #[test]
    fn assemble_strips_frontmatter_and_keeps_contract() {
        use crate::plugin::{RawDoc, RawPage, RawTextItem};
        let raw = RawDoc {
            pages: vec![RawPage {
                items: vec![RawTextItem {
                    text: "---\ntitle: 书名\n---\n\n# 标\n正文\n".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let doc = MarkdownPlugin.assemble(&raw);
        assert_eq!(doc.pages.len(), 1);
        assert!(!doc.pages[0].text.contains("---"), "{:#?}", doc.pages[0].text);
        assert_eq!(kinds(&doc.pages[0].blocks), vec![BlockKind::Heading, BlockKind::Paragraph]);
    }
}

// ---------------------------------------------------------------------------
// frontmatter → DocMeta 通道测试（enrich_meta 后置钩子）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod frontmatter_meta_tests {
    use super::*;
    use crate::engine::DocumentEngine;
    use crate::plugins::text::register_defaults;
    use crate::registry::Registry;
    use crate::plugin::{RawPage, RawTextItem};

    /// md 文本 → 单页单 item 的 RawDoc（enrich_meta 直调用）。
    fn raw_with_text(text: &str) -> RawDoc {
        RawDoc {
            pages: vec![RawPage {
                items: vec![RawTextItem {
                    text: text.to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// engine 全链（register_defaults + DocumentEngine）：含 title/authors/
    /// language 的 md 文件 → extra 三键到位。键名大小写不敏感（`Title` /
    /// `AUTHORS` 照认）、值去包裹引号、authors 逗号拆分后 `"; "` 连接
    /// （对齐 fb2.authors）；正文只剥一次，元数据不进页文本。
    #[test]
    fn frontmatter_metadata_reaches_doc_meta() {
        let p = std::env::temp_dir().join("docforge_md_meta.md");
        std::fs::write(
            &p,
            "---\nTitle: 静夜思\nAUTHORS: 李白, 杜甫\nlanguage: 'zh'\n---\n\n# 题\n正文\n",
        )
        .unwrap();
        let path = p.to_string_lossy().into_owned();
        let mut reg = Registry::new();
        register_defaults(&mut reg);
        let doc = DocumentEngine::new(reg).parse(&path).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(
            doc.meta.extra.get("md.title").map(String::as_str),
            Some("静夜思")
        );
        assert_eq!(
            doc.meta.extra.get("md.authors").map(String::as_str),
            Some("李白; 杜甫")
        );
        assert_eq!(
            doc.meta.extra.get("md.language").map(String::as_str),
            Some("zh")
        );
        assert!(!doc.pages[0].text.contains("静夜思"), "{:?}", doc.pages[0].text);
    }

    /// authors 的方括号列表形态（`[a, b]`，条目可各自带引号）同样拆分连接。
    #[test]
    fn frontmatter_authors_bracket_list_form() {
        let raw = raw_with_text("---\nauthors: [张三, \"李 四\"]\n---\n正文\n");
        let mut doc = MarkdownPlugin.assemble(&raw);
        MarkdownPlugin.enrich_meta(&raw, &mut doc);
        assert_eq!(
            doc.meta.extra.get("md.authors").map(String::as_str),
            Some("张三; 李 四")
        );
    }

    /// 无 frontmatter 的 md：extra 不产任何 `md.*` 键（正文里的 `title: …`
    /// 字样只是段落文本，engine 全链验证）。
    #[test]
    fn no_frontmatter_no_extra_keys() {
        let p = std::env::temp_dir().join("docforge_md_no_meta.md");
        std::fs::write(&p, "# 题\n正文，title: 假键也不认\n").unwrap();
        let path = p.to_string_lossy().into_owned();
        let mut reg = Registry::new();
        register_defaults(&mut reg);
        let doc = DocumentEngine::new(reg).parse(&path).unwrap();
        std::fs::remove_file(&p).ok();
        assert!(
            !doc.meta.extra.keys().any(|k| k.starts_with("md.")),
            "unexpected md.* keys: {:?}",
            doc.meta.extra
        );
    }

    /// 未闭合（50 行内无 `---` 闭合行）：不剥也不取——元数据行留在页文本，
    /// extra 不产键（engine 全链验证钩子与剥离同口径）。
    #[test]
    fn unterminated_frontmatter_not_parsed() {
        let mut content = String::from("---\ntitle: 静夜思\nauthors: 李白\nlanguage: zh\n");
        for i in 0..50 {
            content.push_str(&format!("第{i}行正文\n"));
        }
        let p = std::env::temp_dir().join("docforge_md_unterminated.md");
        std::fs::write(&p, &content).unwrap();
        let path = p.to_string_lossy().into_owned();
        let mut reg = Registry::new();
        register_defaults(&mut reg);
        let doc = DocumentEngine::new(reg).parse(&path).unwrap();
        std::fs::remove_file(&p).ok();
        assert!(doc.pages[0].text.contains("title: 静夜思"), "{:?}", doc.pages[0].text);
        assert!(
            !doc.meta.extra.keys().any(|k| k.starts_with("md.")),
            "unexpected md.* keys: {:?}",
            doc.meta.extra
        );
    }
}
