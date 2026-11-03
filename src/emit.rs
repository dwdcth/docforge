// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! 输出目标：text / markdown（JSON 直接走 serde）。

use crate::model::{BlockKind, Document};

/// 纯文本：逐页 text，页间空行分隔。
pub fn to_text(doc: &Document) -> String {
    doc.pages
        .iter()
        .filter(|p| !p.text.trim().is_empty())
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn table_md(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let norm: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let mut v = r.clone();
            while v.len() < width {
                v.push(String::new());
            }
            v
        })
        .collect();
    let cell = |r: &[String]| format!("| {} |", r.join(" | "));
    let mut out = vec![cell(&norm[0])];
    out.push(format!("| {} |", vec!["---"; width].join(" | ")));
    for r in &norm[1..] {
        out.push(cell(r));
    }
    out.join("\n")
}

fn block_md(b: &crate::model::Block) -> String {
    match b.kind {
        BlockKind::Heading => {
            let lvl = (b.level.unwrap_or(1) as usize).clamp(1, 6);
            format!("{} {}", "#".repeat(lvl), b.content)
        }
        BlockKind::List => {
            let prefix = if b.ordered.unwrap_or(false) {
                "1."
            } else {
                "-"
            };
            b.content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| format!("{prefix} {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        BlockKind::Code => format!(
            "```{}\n{}\n```",
            b.lang.clone().unwrap_or_default(),
            b.content
        ),
        BlockKind::Table => b
            .table
            .as_ref()
            .map(|t| table_md(&t.rows))
            .unwrap_or_default(),
        BlockKind::Quote => b
            .content
            .lines()
            .map(|l| format!("> {l}"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => b.content.clone(),
    }
}

/// Markdown：按块渲染（表格 → Markdown 表格）。
///
/// 相邻同类型列表块（如 HTML 的 `<li>` 逐条产出）合并为一个列表，避免中间插空行
/// 被渲染成多个「松散列表」；跨页不合并。
pub fn to_markdown(doc: &Document) -> String {
    let mut out: Vec<String> = Vec::new();
    for page in &doc.pages {
        let mut prev_list: Option<bool> = None;
        for b in page
            .blocks
            .iter()
            .filter(|b| !b.content.is_empty() || b.table.is_some())
        {
            let rendered = block_md(b);
            if rendered.is_empty() {
                continue;
            }
            if b.kind == BlockKind::List {
                let ordered = b.ordered.unwrap_or(false);
                if prev_list == Some(ordered) {
                    if let Some(last) = out.last_mut() {
                        last.push('\n');
                        last.push_str(&rendered);
                    }
                    continue;
                }
                prev_list = Some(ordered);
            } else {
                prev_list = None;
            }
            out.push(rendered);
        }
    }
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, BlockKind, Document, Page};

    fn list(ordered: bool, content: &str) -> Block {
        Block {
            kind: BlockKind::List,
            content: content.to_string(),
            ordered: Some(ordered),
            ..Default::default()
        }
    }

    #[test]
    fn adjacent_lists_merge_same_kind_only() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![
                    Block {
                        kind: BlockKind::Heading,
                        content: "T".into(),
                        level: Some(2),
                        ..Default::default()
                    },
                    list(false, "a"),
                    list(false, "b"),
                    list(true, "x"),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let md = to_markdown(&doc);
        assert_eq!(md, "## T\n\n- a\n- b\n\n1. x");
    }

    #[test]
    fn table_rendered_as_markdown() {
        let doc = Document {
            pages: vec![Page {
                blocks: vec![Block {
                    kind: BlockKind::Table,
                    table: Some(crate::model::TableData {
                        rows: vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]],
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(to_markdown(&doc), "| a | b |\n| --- | --- |\n| 1 | 2 |");
    }
}
