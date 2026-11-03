// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① feature `encoding` 下 decode 增加 GB18030/Big5 回退（M2）；
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
//! * `text.rs`（本文件）—— 编码健壮读取、txt 段落/章节切分、注册入口；
//! * [`super::chapter`] —— TXT 中文章节切分（零依赖，B 骨架 + 中文词表）；
//! * [`super::markdown`] —— md 块级语法（标题/列表/引用/代码/表格）；
//! * [`super::csv`] —— RFC4180 解析为 `TableData`；
//! * [`super::html`] —— 标准库级标签扫描（标题/段落/列表/代码/表格，跳过 script/style）。

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
/// （feature `encoding`）GB18030 → Big5 → lossy。
pub fn read_text(path: &str) -> Result<String> {
    let raw =
        std::fs::read(path).map_err(|e| DocError::corrupt(format!("cannot read {path}: {e}")))?;
    Ok(decode(raw))
}

/// 字节 → 字符串（BOM 感知，失败退化为 lossy）。
pub fn decode(raw: Vec<u8>) -> String {
    if let Some(rest) = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = raw.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, true);
    }
    if let Some(rest) = raw.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, false);
    }
    match String::from_utf8(raw) {
        Ok(s) => s,
        Err(e) => {
            let bytes = e.as_bytes();
            // 中文编码回退（GBK 家族优先，Big5 次之；各自要求全程无解码错误）
            #[cfg(feature = "encoding")]
            {
                let (s_gb, _, err) = encoding_rs::GB18030.decode(bytes);
                if !err {
                    return s_gb.into_owned();
                }
                let (s_b5, _, err) = encoding_rs::BIG5.decode(bytes);
                if !err {
                    return s_b5.into_owned();
                }
            }
            // 未开 encoding feature：GBK 等单/多字节中文编码退化为替换字符，
            // 中文内容请以 UTF-8 保存（docparser 原注释语义保留）
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| {
            if little_endian {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

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
pub struct TextParseCapability {
    pub id: &'static str,
    pub fmt: &'static str,
}

impl ParseCapability for TextParseCapability {
    fn id(&self) -> &str {
        self.id
    }
    fn formats(&self) -> &[&str] {
        std::slice::from_ref(&self.fmt)
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let content = read_text(path)?;
        let mut items: Vec<RawTextItem> = Vec::new();
        for section in super::chapter::split_chapters(&content) {
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
    fn parse(&self, path: &str) -> Result<RawDoc> {
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
    fn parse(&self, path: &str) -> Result<RawDoc> {
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
    fn decode_handles_bom_and_utf16() {
        let mut utf8_bom = vec![0xEF, 0xBB, 0xBF];
        utf8_bom.extend_from_slice("你好".as_bytes());
        assert_eq!(decode(utf8_bom), "你好");

        let mut utf16 = vec![0xFF, 0xFE];
        for u in "ab".encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode(utf16), "ab");

        // 非法 UTF-8 不 panic，退化为替换字符
        assert!(decode(vec![0x61, 0xFF, 0x62]).contains('a'));
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
    fn gb18030_decoded_when_feature() {
        // "你好，世界" 的 GBK 编码（非 UTF-8，无 BOM）
        let gbk: Vec<u8> = vec![0xC4, 0xE3, 0xBA, 0xC3, 0xA3, 0xAC, 0xCA, 0xC0, 0xBD, 0xE7];
        assert_eq!(decode(gbk), "你好，世界");
        // UTF-8 优先不受影响
        assert_eq!(decode("常规".as_bytes().to_vec()), "常规");
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

        let raw = TextParseCapability { id: "text.read.txt", fmt: "txt" }
            .parse(p.to_str().unwrap())
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

        let raw = TextParseCapability { id: "text.read.txt", fmt: "txt" }
            .parse(p.to_str().unwrap())
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
}
