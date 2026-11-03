// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① feature `encoding` 下编码回退——chardetng 概率探测
//    （置信闸门，覆盖 Big5/Shift-JIS/EUC-KR/Windows-125x 等）+ GB18030/Big5
//    固定回退（M2）；解码链后抽出为横切能力模块 [`crate::encoding`]（本文件
//    以 `pub use` 转发 [`decode`]，旧调用点零改动）；
// ② parse 集成中文章节切分（chapter.rs：章标题产 level=1 item；章内小节
//    产 level=2/3 item，小节行从段落剥离，按行号回插保序）；
// ③ assemble 将 level 映射为 Heading 块；④ feature `lang-detect` 下
// default_process_chain 追加语言检测；⑤ register_defaults 注册 chapter 无需
// （纯函数模块）。算法骨架另吸收 document-parser（见 chapter.rs 头）。
//! 纯文本族插件：txt / md / csv / html（零第三方依赖，语义对齐 Python 参考实现）。
//!
//! 这四类**不走 PDF**：没有布局与坐标，转 PDF 是纯粹的有损往返，直接按语法解。
//!
//! 分工：
//! * `text.rs`（本文件）—— txt 段落/章节切分、注册入口（编码健壮读取的
//!   解码链在 [`crate::encoding`]，本文件 `pub use` 转发 [`decode`]）；
//! * [`super::chapter`] —— TXT 中文章节切分（零依赖，B 骨架 + 中文词表，
//!   规则可用 [`super::chapter::ChapterRules`] 覆盖/补充）；
//! * [`super::markdown`] —— md 块级语法（标题/列表/引用/代码/表格）；
//! * [`super::csv`] —— RFC4180 解析为 `TableData`；
//! * [`super::html`] —— 标准库级标签扫描（标题/段落/列表/代码/表格，跳过 script/style）。

use std::path::Path;
use std::sync::Arc;

use crate::error::{DocError, Result};
use crate::model::{Block, BlockKind, BlockSource, Document, Page};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawPage, RawTextItem};
use crate::registry::Registry;

#[cfg(feature = "csv")]
use super::csv;
#[cfg(feature = "html")]
use super::html;
#[cfg(feature = "md")]
use super::markdown;

/// 编码健壮读取：UTF-8（含 BOM）→ UTF-16（BOM）→ UTF-8 strict →
/// （feature `encoding`）chardetng 概率探测（置信闸门）→ GB18030 → Big5 → lossy。
/// 解码链实现见 [`crate::encoding`]。`path` 吃任何 `AsRef<Path>`。
pub fn read_text(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    let raw = std::fs::read(path)
        .map_err(|e| DocError::corrupt(format!("cannot read {}: {e}", path.display())))?;
    Ok(crate::encoding::decode(raw))
}

/// 字节 → 字符串（BOM 感知，失败退化为 lossy）。
///
/// 转发到 [`crate::encoding::decode`]（横切能力模块化后的对外兼容 re-export，
/// 旧调用点 `plugins::text::decode` 零改动）。编码名探测见
/// [`crate::encoding::detect_and_decode`]。
pub use crate::encoding::decode;

/// 按空行切段落（与 Python `_split_paragraphs` 对齐：段内单换行保留）。
pub fn split_paragraphs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !buf.is_empty() {
                out.push(buf.join("\n").trim().to_string());
                buf.clear();
            }
        } else {
            buf.push(line);
        }
    }
    if !buf.is_empty() {
        out.push(buf.join("\n").trim().to_string());
    }
    out.retain(|s| !s.is_empty());
    out
}

/// 段缓冲冲刷为段落 item（与 `split_paragraphs` 的 join/trim/去空语义一致）。
fn flush_paragraph(buf: &mut Vec<&str>, items: &mut Vec<RawTextItem>) {
    if buf.is_empty() {
        return;
    }
    let para = buf.join("\n").trim().to_string();
    buf.clear();
    if !para.is_empty() {
        items.push(RawTextItem {
            text: para,
            source: BlockSource::Native,
            ..Default::default()
        });
    }
}

/// 单格式纯文本解析后端（txt）。
///
/// `rules` 控制 TXT 章节切分（内置规则 + 自定义覆盖/补充，见
/// [`super::chapter::ChapterRules`]）；`Default` 即默认装配（id/fmt 取 txt
/// 内置值、rules 走内置规则），构造点可用 `..Default::default()` 补字段。
pub struct TextParseCapability {
    pub id: &'static str,
    pub fmt: &'static str,
    /// 章节切分规则（默认 [`super::chapter::ChapterRules::default`]，即纯内置规则）
    pub rules: super::chapter::ChapterRules,
}

impl Default for TextParseCapability {
    fn default() -> Self {
        Self {
            id: "text.read.txt",
            fmt: "txt",
            rules: super::chapter::ChapterRules::default(),
        }
    }
}

impl ParseCapability for TextParseCapability {
    fn id(&self) -> &str {
        self.id
    }
    fn formats(&self) -> &[&str] {
        std::slice::from_ref(&self.fmt)
    }
    fn parse(&self, path: &Path) -> Result<RawDoc> {
        let content = read_text(path)?;
        let mut items: Vec<RawTextItem> = Vec::new();
        for section in super::chapter::split_chapters_with(&content, &self.rules) {
            if let Some(h) = &section.heading {
                // 章标题 → Heading（level=1；assemble 映射为 Heading 块）
                items.push(RawTextItem {
                    text: h.title.clone(),
                    level: Some(h.level),
                    source: BlockSource::Native,
                    ..Default::default()
                });
            }
            // 章内小节（level 2/3）不切段：小节行从段落文本剥离，按行号
            // 回插为 Heading item，保持与前后的段落相对顺序。
            let subs: std::collections::BTreeMap<usize, &super::chapter::Subheading> = section
                .subheadings
                .iter()
                .map(|s| (s.line_index, s))
                .collect();
            let mut buf: Vec<&str> = Vec::new();
            for (i, line) in content
                .lines()
                .enumerate()
                .skip(section.body.start)
                .take(section.body.end.saturating_sub(section.body.start))
            {
                if let Some(sub) = subs.get(&i) {
                    flush_paragraph(&mut buf, &mut items);
                    items.push(RawTextItem {
                        text: sub.title.clone(),
                        level: Some(sub.level),
                        source: BlockSource::Native,
                        ..Default::default()
                    });
                } else if line.trim().is_empty() {
                    flush_paragraph(&mut buf, &mut items);
                } else {
                    buf.push(line);
                }
            }
            flush_paragraph(&mut buf, &mut items);
        }
        Ok(RawDoc {
            pages: vec![RawPage {
                name: None,
                items,
                ..Default::default()
            }],
            ..Default::default()
        })
    }
}

/// txt 插件：RawDoc（段）→ IR（每段一个 Paragraph 块）。
pub struct TextPlugin;

impl FormatPlugin for TextPlugin {
    fn id(&self) -> &str {
        "txt"
    }
    fn formats(&self) -> &[&str] {
        &["txt"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["text.read.txt"]
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        #[cfg(feature = "lang-detect")]
        {
            vec!["lang-detect"]
        }
        #[cfg(not(feature = "lang-detect"))]
        {
            vec![]
        }
    }
    fn assemble(&self, raw: &RawDoc) -> Document {
        let items: Vec<&RawTextItem> = raw
            .pages
            .first()
            .map(|p| p.items.iter().collect())
            .unwrap_or_default();
        let blocks: Vec<Block> = items
            .iter()
            .filter(|i| !i.text.trim().is_empty())
            .map(|i| Block {
                kind: if i.level.is_some() { BlockKind::Heading } else { BlockKind::Paragraph },
                content: i.text.clone(),
                level: i.level,
                source: i.source,
                confidence: i.confidence,
                ..Default::default()
            })
            .collect();
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        Document {
            pages: vec![Page {
                blocks,
                text: texts.join("\n\n"),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
}

/// 原文整体交给插件自解析的解析后端（md / html）。
pub struct WholeTextParseCapability {
    pub id: &'static str,
    pub fmt: &'static str,
}

impl ParseCapability for WholeTextParseCapability {
    fn id(&self) -> &str {
        self.id
    }
    fn formats(&self) -> &[&str] {
        std::slice::from_ref(&self.fmt)
    }
    fn parse(&self, path: &Path) -> Result<RawDoc> {
        let content = read_text(path)?;
        Ok(RawDoc {
            pages: vec![RawPage {
                items: vec![RawTextItem {
                    text: content,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        })
    }
}

/// CSV 解析后端：RFC4180 → `RawPage.tables`。
#[cfg(feature = "csv")]
pub struct CsvParseCapability;

#[cfg(feature = "csv")]
impl ParseCapability for CsvParseCapability {
    fn id(&self) -> &str {
        "text.read.csv"
    }
    fn formats(&self) -> &[&str] {
        &["csv"]
    }
    fn parse(&self, path: &Path) -> Result<RawDoc> {
        let content = read_text(path)?;
        let rows = csv::parse_rows(&content);
        Ok(RawDoc {
            pages: vec![RawPage {
                tables: if rows.is_empty() { vec![] } else { vec![rows] },
                ..Default::default()
            }],
            ..Default::default()
        })
    }
}

/// 注册文本族后端与插件（txt 恒开；md / csv / html 随 feature）。
///
/// 这是内置文本能力的唯一注册入口，CLI / SDK / Python 绑定都调它。
pub fn register_defaults(reg: &mut Registry) {
    let _ = reg.register_parse(
        Arc::new(TextParseCapability {
            id: "text.read.txt",
            fmt: "txt",
            ..Default::default()
        }),
        None,
    );
    #[cfg(feature = "md")]
    {
        let _ = reg.register_parse(
            Arc::new(WholeTextParseCapability {
                id: "text.read.md",
                fmt: "md",
            }),
            None,
        );
        reg.register_plugin(Arc::new(markdown::MarkdownPlugin));
    }
    #[cfg(feature = "csv")]
    {
        let _ = reg.register_parse(Arc::new(CsvParseCapability), None);
        reg.register_plugin(Arc::new(csv::CsvPlugin));
    }
    #[cfg(feature = "html")]
    {
        let _ = reg.register_parse(
            Arc::new(WholeTextParseCapability {
                id: "html.parse",
                fmt: "html",
            }),
            None,
        );
        reg.register_plugin(Arc::new(html::HtmlPlugin));
    }

    reg.register_plugin(Arc::new(TextPlugin));

    // 字幕（srt/vtt/ass/ssa）：无第三方依赖，跟随 txt 始终注册
    super::subtitle::register_defaults(reg);

    // 语言检测处理能力：随 feature `lang-detect`（M2）
    #[cfg(feature = "lang-detect")]
    super::lang_detect::register(reg);

    // RTF：控制字状态机，随 feature `rtf`（默认开）
    #[cfg(feature = "rtf")]
    super::rtf::register_defaults(reg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_split_on_blank_lines() {
        assert_eq!(
            split_paragraphs("a\nb\n\nc\n\n\n d "),
            vec!["a\nb", "c", "d"]
        );
        assert_eq!(split_paragraphs("   \n\n"), Vec::<String>::new());
    }

    #[test]
    fn txt_plugin_splits_paragraphs() {
        let raw = RawDoc {
            pages: vec![RawPage {
                items: vec![
                    RawTextItem {
                        text: "p1".into(),
                        ..Default::default()
                    },
                    RawTextItem {
                        text: "p2".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let doc = TextPlugin.assemble(&raw);
        assert_eq!(doc.pages[0].blocks.len(), 2);
        assert_eq!(doc.pages[0].text, "p1\n\np2");
    }
}

#[cfg(test)]
mod m2_tests {
    use super::*;

    #[test]
    #[cfg(feature = "encoding")]
    fn gbk_markdown_decoded() {
        // GBK 字节的 md（非 UTF-8，无 BOM）：md 走 WholeTextParseCapability，
        // 与 txt/csv/html 共用同一条 read_text → crate::encoding::decode 链。
        // decode 本体的直连断言（含 chardetng/GB18030/Big5/置信闸门）已随
        // 解码链迁至 crate::encoding 的测试。
        let md = "# 中文标题\n\n正文段落。";
        let (bytes, _, had_errors) = encoding_rs::GB18030.encode(md);
        let bytes = bytes.into_owned();
        assert!(!had_errors);

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("enc-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("note.md");
        std::fs::write(&p, &bytes).unwrap();

        let raw = WholeTextParseCapability { id: "text.read.md", fmt: "md" }
            .parse(&p)
            .unwrap();
        assert_eq!(raw.pages[0].items[0].text, md);
        // read_text 直连同链：BOM 无、UTF-16 无、UTF-8 strict 失败 → 编码回退
        assert_eq!(read_text(&p).unwrap(), md);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chapters_become_heading_blocks() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("m2-txt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("novel.txt");
        std::fs::write(
            &p,
            "第一章 出发\n\n清晨他离开村庄。\n\n第二章 归来\n\n多年以后他回来了。",
        )
        .unwrap();

        let raw = TextParseCapability { id: "text.read.txt", fmt: "txt", ..Default::default() }
            .parse(&p)
            .unwrap();
        let doc = TextPlugin.assemble(&raw);
        let blocks = &doc.pages[0].blocks;
        assert_eq!(blocks[0].kind, crate::model::BlockKind::Heading);
        assert_eq!(blocks[0].level, Some(1));
        assert_eq!(blocks[0].content, "第一章 出发");
        assert_eq!(blocks[1].content, "清晨他离开村庄。");
        assert_eq!(blocks[2].content, "第二章 归来");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subsection_inside_chapter_end_to_end() {
        // 章内小节端到端：章标题 level 1、小节行 level 2 且从段落剥离，
        // 小节 item 按行号回插，保持与段落的相对顺序
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("m2-txt-sub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("manual.txt");
        std::fs::write(
            &p,
            "第一章 部署\n\n1.1 准备\n\n安装依赖。\n\n1.2 启动\n\n运行服务。\n\n第二章 验收\n\n全部通过。",
        )
        .unwrap();

        let raw = TextParseCapability { id: "text.read.txt", fmt: "txt", ..Default::default() }
            .parse(&p)
            .unwrap();
        let doc = TextPlugin.assemble(&raw);
        let blocks = &doc.pages[0].blocks;
        let contents: Vec<&str> = blocks.iter().map(|b| b.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "第一章 部署",
                "1.1 准备",
                "安装依赖。",
                "1.2 启动",
                "运行服务。",
                "第二章 验收",
                "全部通过。",
            ]
        );
        assert_eq!(blocks[0].kind, crate::model::BlockKind::Heading);
        assert_eq!(blocks[0].level, Some(1));
        assert_eq!(blocks[1].kind, crate::model::BlockKind::Heading);
        assert_eq!(blocks[1].level, Some(2));
        assert_eq!(blocks[2].kind, crate::model::BlockKind::Paragraph);
        assert_eq!(blocks[3].level, Some(2));
        assert_eq!(blocks[5].level, Some(1));
        assert_eq!(blocks[6].kind, crate::model::BlockKind::Paragraph);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_chapter_rules_flow_through_plugin() {
        // 自定义规则经 TextParseCapability.rules 生效（parse 传规则给
        // split_chapters_with）：level 2 自定义小节行从段落剥离、按行号
        // 回插为 Heading item；默认规则构造点不受影响（前两个测试回归）。
        use crate::plugins::chapter::{ChapterRules, CustomRule};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("m2-txt-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("manual.txt");
        std::fs::write(
            &p,
            "第一章 部署\n\nStep 1 准备\n\n安装依赖。\n\nStep 2 启动\n\n运行服务。",
        )
        .unwrap();

        let raw = TextParseCapability {
            id: "text.read.txt",
            fmt: "txt",
            rules: ChapterRules {
                extra: vec![CustomRule { prefix: "Step ".to_string(), level: 2 }],
                replace_builtin: false,
            },
        }
        .parse(&p)
        .unwrap();
        let doc = TextPlugin.assemble(&raw);
        let blocks = &doc.pages[0].blocks;
        assert_eq!(blocks[0].content, "第一章 部署");
        assert_eq!(blocks[0].level, Some(1));
        assert_eq!(blocks[1].content, "Step 1 准备");
        assert_eq!(blocks[1].kind, crate::model::BlockKind::Heading);
        assert_eq!(blocks[1].level, Some(2));
        assert_eq!(blocks[2].content, "安装依赖。");
        assert_eq!(blocks[3].content, "Step 2 启动");
        assert_eq!(blocks[3].level, Some(2));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
