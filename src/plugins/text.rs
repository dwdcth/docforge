// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! 纯文本族插件：txt / md / csv / html（零第三方依赖，语义对齐 Python 参考实现）。
//!
//! 这四类**不走 PDF**：没有布局与坐标，转 PDF 是纯粹的有损往返，直接按语法解。
//!
//! 分工：
//! * `text.rs`（本文件）—— 编码健壮读取、txt 段落切分、注册入口；
//! * [`super::markdown`] —— md 块级语法（标题/列表/引用/代码/表格）；
//! * [`super::csv`] —— RFC4180 解析为 `TableData`；
//! * [`super::html`] —— 标准库级标签扫描（标题/段落/列表/代码/表格，跳过 script/style）。

use std::sync::Arc;

use crate::error::{DocError, Result};
use crate::model::{Block, BlockKind, Document, Page};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawPage, RawTextItem};
use crate::registry::Registry;

#[cfg(feature = "csv")]
use super::csv;
#[cfg(feature = "html")]
use super::html;
#[cfg(feature = "md")]
use super::markdown;

/// 编码健壮读取：UTF-8（含 BOM）→ UTF-16（BOM）→ UTF-8 lossy。
///
/// 与 Python 参考实现（`utf-8-sig → gbk → latin-1`）的差异：Rust 核心不引入
/// `encoding_rs`（保持零第三方依赖），GBK 等单/多字节中文编码会退化为替换字符。
/// 中文内容请以 UTF-8 保存；需要 GBK 时由调用方先行转码。
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
        // 非法字节逐段替换，保留可读部分（与 lossy 语义一致）
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
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
        let items: Vec<RawTextItem> = split_paragraphs(&content)
            .into_iter()
            .map(|t| RawTextItem {
                text: t,
                ..Default::default()
            })
            .collect();
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
        vec![]
    }
    fn assemble(&self, raw: &RawDoc) -> Document {
        let items: Vec<&str> = raw
            .pages
            .first()
            .map(|p| p.items.iter().map(|i| i.text.as_str()).collect())
            .unwrap_or_default();
        let blocks: Vec<Block> = items
            .iter()
            .filter(|t| !t.trim().is_empty())
            .map(|t| Block {
                kind: BlockKind::Paragraph,
                content: (*t).to_string(),
                ..Default::default()
            })
            .collect();
        Document {
            pages: vec![Page {
                blocks,
                text: items.join("\n\n"),
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

    // RTF：控制词状态机，随 feature `rtf`（默认开）
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
