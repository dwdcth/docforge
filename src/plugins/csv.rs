// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! CSV 解析（RFC4180 语义）：双引号包裹、`""` 转义、字段内逗号/换行、CRLF。
//!
//! 对齐 Python `csv.reader` 的常用行为：空行不算一行记录。
//! 不做分隔符嗅探（`,` 固定），也不做类型推断——表格原样进 IR。

use crate::model::{Block, BlockKind, Document, Page, TableData};
use crate::plugin::{FormatPlugin, RawDoc};

/// 解析 CSV 文本为行 × 列。
pub fn parse_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut row_touched = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' if field.is_empty() && !in_quotes => {
                in_quotes = true;
                row_touched = true;
            }
            ',' if !in_quotes => {
                row.push(std::mem::take(&mut field));
                row_touched = true;
            }
            '\r' if !in_quotes => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                push_row(&mut rows, &mut row, &mut row_touched);
            }
            '\n' if !in_quotes => {
                row.push(std::mem::take(&mut field));
                push_row(&mut rows, &mut row, &mut row_touched);
            }
            _ => {
                field.push(c);
                row_touched = true;
            }
        }
    }
    if row_touched || !field.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// 收行：丢弃整行为空的记录（对齐 `csv.reader` 对空行的处理）。
fn push_row(rows: &mut Vec<Vec<String>>, row: &mut Vec<String>, row_touched: &mut bool) {
    let empty = row.len() == 1 && row[0].is_empty();
    if !empty {
        rows.push(std::mem::take(row));
    } else {
        row.clear();
    }
    *row_touched = false;
}

/// csv 插件：一页一个表格块。
pub struct CsvPlugin;

impl FormatPlugin for CsvPlugin {
    fn id(&self) -> &str {
        "csv"
    }
    fn formats(&self) -> &[&str] {
        &["csv"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["text.read.csv"]
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        vec![]
    }
    fn assemble(&self, raw: &RawDoc) -> Document {
        let rows: Vec<Vec<String>> = raw
            .pages
            .first()
            .and_then(|p| p.tables.first().cloned())
            .unwrap_or_default();
        let text = rows
            .iter()
            .map(|r| r.join(","))
            .collect::<Vec<_>>()
            .join("\n");
        let blocks = if rows.is_empty() {
            vec![]
        } else {
            vec![Block {
                kind: BlockKind::Table,
                content: rows[0].join(" | "),
                table: Some(TableData {
                    rows,
                    ..Default::default()
                }),
                ..Default::default()
            }]
        };
        Document {
            pages: vec![Page {
                blocks,
                text,
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
    fn plain_rows() {
        assert_eq!(
            parse_rows("a,b\n1,2\n"),
            vec![vec!["a", "b"], vec!["1", "2"]]
        );
    }

    #[test]
    fn quoted_field_with_comma_and_newline() {
        let rows = parse_rows("name,note\n\"张,三\",\"第一行\n第二行\"\n");
        assert_eq!(rows[1][0], "张,三");
        assert_eq!(rows[1][1], "第一行\n第二行");
    }

    #[test]
    fn escaped_quotes_and_crlf() {
        let rows = parse_rows("a,\"say \"\"hi\"\"\"\r\nb,c\r\n");
        assert_eq!(rows[0], vec!["a", "say \"hi\""]);
        assert_eq!(rows[1], vec!["b", "c"]);
    }

    #[test]
    fn blank_lines_are_skipped() {
        assert_eq!(parse_rows("a\n\nb\n"), vec![vec!["a"], vec!["b"]]);
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(parse_rows("").is_empty());
    }

    #[test]
    fn plugin_emits_table_block() {
        let raw = RawDoc {
            pages: vec![crate::plugin::RawPage {
                tables: vec![vec![
                    vec!["h1".into(), "h2".into()],
                    vec!["1".into(), "2".into()],
                ]],
                ..Default::default()
            }],
            ..Default::default()
        };
        let doc = CsvPlugin.assemble(&raw);
        assert_eq!(doc.pages[0].blocks.len(), 1);
        assert_eq!(doc.pages[0].blocks[0].kind, BlockKind::Table);
        assert_eq!(doc.pages[0].text, "h1,h2\n1,2");
    }
}
