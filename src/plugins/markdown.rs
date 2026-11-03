// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! Markdown 块级解析：直接解语法，不渲染成 PDF 再反推。
//!
//! 对齐 Python `MdParseCapability._parse_md`：标题 / 无序列表 / 有序列表 / 引用 /
//! 代码围栏 / 管道表格 / 段落（连续行以空格合并）。
//! 这是**块级**解析，不做行内语法（粗体/链接/行内代码）渲染，`content` 保留原文。

use crate::model::{Block, BlockKind, Document, Page, TableData};
use crate::plugin::{FormatPlugin, RawDoc};

/// `#{1,6} ` 标题。
fn heading(line: &str) -> Option<(u8, String)> {
    let t = line.trim_start();
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    if !(rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    Some((hashes as u8, rest.trim().to_string()))
}

/// `> ` 引用。
fn quote(line: &str) -> Option<String> {
    let t = line.trim_start();
    let rest = t.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest).trim().to_string())
}

/// `- / * / + ` 无序列表项。
fn ulist_item(line: &str) -> Option<String> {
    let t = line.trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// `1. / 1)` 有序列表项。
fn olist_item(line: &str) -> Option<String> {
    let t = line.trim_start();
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = &t[digits..];
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    if !(rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    Some(rest.trim().to_string())
}

/// 表格分隔行：仅由 `| - : 空白` 组成，且同时含 `-` 与 `|`。
fn is_table_sep(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
        && t.contains('-')
        && t.contains('|')
}

fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn flush_para(blocks: &mut Vec<Block>, para: &mut Vec<String>) {
    if para.is_empty() {
        return;
    }
    let content = para.join(" ");
    para.clear();
    let content = content.trim();
    if !content.is_empty() {
        blocks.push(Block {
            kind: BlockKind::Paragraph,
            content: content.to_string(),
            ..Default::default()
        });
    }
}

/// 解析 Markdown 文本为块序列。
pub fn parse(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let mut blocks: Vec<Block> = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut i = 0usize;

    while i < n {
        let line = lines[i];
        let trimmed = line.trim_start();

        // 代码围栏
        if trimmed.starts_with("```") {
            flush_para(&mut blocks, &mut para);
            let lang = trimmed.trim_start_matches('`').trim().to_string();
            let mut buf: Vec<&str> = Vec::new();
            i += 1;
            while i < n && !lines[i].trim_start().starts_with("```") {
                buf.push(lines[i]);
                i += 1;
            }
            if i < n {
                i += 1; // 结束围栏
            }
            blocks.push(Block {
                kind: BlockKind::Code,
                content: buf.join("\n"),
                lang: if lang.is_empty() { None } else { Some(lang) },
                ..Default::default()
            });
            continue;
        }

        if line.trim().is_empty() {
            flush_para(&mut blocks, &mut para);
            i += 1;
            continue;
        }

        if let Some((level, content)) = heading(line) {
            flush_para(&mut blocks, &mut para);
            blocks.push(Block {
                kind: BlockKind::Heading,
                content,
                level: Some(level),
                ..Default::default()
            });
            i += 1;
            continue;
        }

        if let Some(content) = quote(line) {
            flush_para(&mut blocks, &mut para);
            blocks.push(Block {
                kind: BlockKind::Quote,
                content,
                ..Default::default()
            });
            i += 1;
            continue;
        }

        // 管道表格（必须有分隔行，否则按段落处理）
        if trimmed.starts_with('|') && i + 1 < n && is_table_sep(lines[i + 1]) {
            flush_para(&mut blocks, &mut para);
            let header = split_row(line);
            let mut rows: Vec<Vec<String>> = vec![header.clone()];
            i += 2;
            while i < n && lines[i].trim_start().starts_with('|') {
                rows.push(split_row(lines[i]));
                i += 1;
            }
            blocks.push(Block {
                kind: BlockKind::Table,
                content: header.join(" | "),
                table: Some(TableData {
                    rows,
                    ..Default::default()
                }),
                ..Default::default()
            });
            continue;
        }

        if let Some(first) = ulist_item(line) {
            flush_para(&mut blocks, &mut para);
            let mut items = vec![first];
            i += 1;
            while i < n {
                match ulist_item(lines[i]) {
                    Some(it) => {
                        items.push(it);
                        i += 1;
                    }
                    None => break,
                }
            }
            blocks.push(Block {
                kind: BlockKind::List,
                content: items.join("\n"),
                ordered: Some(false),
                ..Default::default()
            });
            continue;
        }

        if let Some(first) = olist_item(line) {
            flush_para(&mut blocks, &mut para);
            let mut items = vec![first];
            i += 1;
            while i < n {
                match olist_item(lines[i]) {
                    Some(it) => {
                        items.push(it);
                        i += 1;
                    }
                    None => break,
                }
            }
            blocks.push(Block {
                kind: BlockKind::List,
                content: items.join("\n"),
                ordered: Some(true),
                ..Default::default()
            });
            continue;
        }

        para.push(line.trim().to_string());
        i += 1;
    }

    flush_para(&mut blocks, &mut para);
    blocks
}

/// md 插件：原文交给 [`parse`]，整页一组块。
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
        Document {
            pages: vec![Page {
                blocks: parse(text),
                text: text.to_string(),
                ..Default::default()
            }],
            ..Default::default()
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
