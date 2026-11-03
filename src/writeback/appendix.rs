//! PDF 双语附录载荷：「原 PDF 不动、尾部追加译文页」合作分工的 docforge
//! 侧——产出结构化译文序列（按原文页分组），PDF 页生成在消费者侧（rumi
//! 的 oxidize-pdf 双层 PDF 经验）。
//!
//! # 合作分工与边界
//!
//! PDF 回写（格内替换）不现实：PDF 无重排能力，译后文本长度与原布局必然
//! 失配。可行的合作形态是**附录页**：原 PDF 字节不动，在尾部追加译文页。
//! 分工——
//!
//! * docforge（本模块）：把命中的译文按 [`crate::emit::TranslationUnit`]
//!   锚点整理成 [`AppendixPage`] 序列（结构化消费主形态）或人读 Markdown
//!   （[`to_appendix_markdown`]，调试 / 快照用）；
//! * 消费者（rumi）：拿序列渲染追加 PDF 页——一页对一页，或按内容高度
//!   自适应再分页（消费者自由）。
//!
//! **为什么 docforge 不产 PDF**：不为 PDF 写引入新依赖（本 crate 的依赖
//! 取舍是只进解析所需）；且渲染字号 / 分页 / 原页底层叠加（双层对照）是
//! 消费者的领域知识。docforge 只负责结构化事实——哪些单元、原文是什么、
//! 译文是什么、属于哪一页。
//!
//! # 对齐契约（与回写侧同口径）
//!
//! * 译文查找语义与 writeback 一致：id 精确匹配、重复 id 后者胜（HashMap）、
//!   空白译文视为未提供（复用 [`super::translation_map`]）；
//! * 未命中的单元不收——原 PDF 本来就不动，缺译文即该处无对照；
//! * 不做 kind 过滤：凡 [`crate::emit::to_translation_units`] 发出的单元
//!   （段落 / 标题 / 列表 / 表格单元格……）都进序列，消费者按需取舍。

use crate::emit::TranslationUnit;

/// 一条译文对照：unit 锚点 + 原 / 译文本对。
///
/// `unit_id` 是溯源锚点（[`TranslationUnit::id`]，`p{页}-b{块序}`），消费
/// 者可回查原块；`original` 取单元原文零失真，`translation` 已裁剪首尾
/// 空白（翻译端常见的尾换行不进载荷，与回写侧译文口径一致）。
#[derive(Debug, Clone, PartialEq)]
pub struct AppendixEntry {
    /// [`TranslationUnit::id`]（溯源锚点，消费者可回查块）。
    pub unit_id: String,
    /// [`TranslationUnit::text`]（原文，原样透传）。
    pub original: String,
    /// 命中的译文（首尾空白已裁剪）。
    pub translation: String,
}

/// 一个附录页：对应原文档的一页（`source_page` =
/// [`TranslationUnit::page`]，即原文页名）。
///
/// rumi 渲染追加 PDF 页时一页对一页（或按内容高度自适应再分页——消费者
/// 自由）。分组键是页名：同名页合入同组；`None` 容纳无页名的退化形态
/// （[`crate::emit::to_translation_units`] 产出的单元恒有名，页序兜底）。
#[derive(Debug, Clone, PartialEq)]
pub struct AppendixPage {
    /// 对应的原文页名（无则 `None`）。
    pub source_page: Option<String>,
    /// 该页的译文对照（文档序）。
    pub entries: Vec<AppendixEntry>,
}

/// 收集命中的译文，按原文页分组为附录页序列。
///
/// * 页组按首次命中序排列（即文档序），同页单元合入同组、组内保持文档序；
/// * 未命中 / 空白译文的单元不收；无命中的页不产空组——空结果 = 无一命中；
/// * `original` 取单元原文（零失真）；`translation` 裁剪首尾空白后收下。
pub fn to_appendix_pages(
    units: &[TranslationUnit],
    translated: &[(String, String)],
) -> Vec<AppendixPage> {
    let map = super::translation_map(translated);
    let mut pages: Vec<AppendixPage> = Vec::new();
    // 页名 → 页组下标（同名页合入同组；首次命中即建组，保持文档序）
    let mut index: std::collections::HashMap<Option<&str>, usize> =
        std::collections::HashMap::new();
    for unit in units {
        let Some(tr) = map.get(unit.id.as_str()).copied() else {
            continue; // 未命中：不收
        };
        let tr = tr.trim();
        if tr.is_empty() {
            continue; // 空白译文视为未提供（后者胜之后再判，口径见上）
        }
        let key = unit.page.as_deref();
        let idx = *index.entry(key).or_insert_with(|| {
            pages.push(AppendixPage {
                source_page: unit.page.clone(),
                entries: Vec::new(),
            });
            pages.len() - 1
        });
        pages[idx].entries.push(AppendixEntry {
            unit_id: unit.id.clone(),
            original: unit.text.clone(),
            translation: tr.to_string(),
        });
    }
    pages
}

/// 附录页的人读 / 调试 Markdown 形态（结构化消费主推 [`AppendixPage`]）。
///
/// * 首行 `# 译文附录`；每源页一节 `## 第 {page} 页`（`source_page` 为
///   `None` → `## （无页名）`）；
/// * 条目 = 原文引用 + 译文正文：原文逐行加 `> ` 前缀（与 [`crate::emit`]
///   的 Quote 渲染同构，多行原文即多行引用），译文正文行紧随其后；
///   条目之间空行；
/// * 无条目的页跳过；空输入（或全为无条目页）返回空串。
pub fn to_appendix_markdown(pages: &[AppendixPage]) -> String {
    let mut blocks: Vec<String> = vec!["# 译文附录".to_string()];
    for page in pages {
        if page.entries.is_empty() {
            continue;
        }
        blocks.push(match page.source_page.as_deref() {
            Some(name) => format!("## 第 {name} 页"),
            None => "## （无页名）".to_string(),
        });
        for entry in &page.entries {
            let mut block = String::new();
            for line in entry.original.lines() {
                block.push_str("> ");
                block.push_str(line);
                block.push('\n');
            }
            block.push_str(&entry.translation);
            blocks.push(block);
        }
    }
    if blocks.len() == 1 {
        return String::new(); // 除首行外无任何内容：不产悬空标题
    }
    blocks.join("\n\n")
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::to_translation_units;
    use std::path::{Path, PathBuf};

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_appendix_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 真实管线取 units。任务约束：`TranslationUnit` 正被并行加字段
    ///（header），测试一律不手写其字面量——临时件 → `docforge::parse` →
    /// `to_translation_units`。
    fn units_of(path: &Path) -> Vec<TranslationUnit> {
        to_translation_units(&crate::parse(&path).unwrap())
    }

    const SRT: &str = "\
1
00:00:01,000 --> 00:00:03,000
山有木兮木有枝。

2
00:00:04,000 --> 00:00:06,000
心悦君兮
君不知。

3
00:00:07,000 --> 00:00:09,000
愿言配德兮。

4
00:00:10,000 --> 00:00:12,000
心欲颠兮。
";

    /// 分组正确（srt：一条字幕一页，页名 = 时间轴）：半数单元给译文 →
    /// 按页分组、文档序、未命中页不收、错锚不收。
    #[test]
    fn groups_by_source_page_in_document_order() {
        let path = case_dir("groups").join("in.srt");
        std::fs::write(&path, SRT).unwrap();
        let units = units_of(&path);
        assert_eq!(units.len(), 4, "四条字幕 → 四个单元（各占一页）");

        // 单元 0/1/3 给译文（半数命中口径），单元 2 不给；外加错锚一条
        let translated = vec![
            (units[0].id.clone(), format!("译-{}", units[0].text)),
            (units[1].id.clone(), format!("译-{}", units[1].text)),
            (units[3].id.clone(), format!("译-{}", units[3].text)),
            ("p9-b9".to_string(), "错锚不收".to_string()),
        ];
        let pages = to_appendix_pages(&units, &translated);

        assert_eq!(pages.len(), 3, "单元 2 所在页无命中 → 不产空组");
        for (i, u) in [0, 1, 3].into_iter().enumerate() {
            assert_eq!(pages[i].source_page, units[u].page, "页名取单元 page");
            assert_eq!(pages[i].entries.len(), 1);
            let e = &pages[i].entries[0];
            assert_eq!(e.unit_id, units[u].id);
            assert_eq!(e.original, units[u].text, "原文零失真透传");
            assert_eq!(e.translation, format!("译-{}", units[u].text));
        }
        assert!(
            pages.iter().all(|p| p.source_page != units[2].page),
            "未命中页不收"
        );
    }

    /// markdown 形态快照（全串比对）：多行原文逐行加 `> `，条目间空行，
    /// 页节 `## 第 {page} 页`。
    #[test]
    fn markdown_snapshot_via_real_pipeline() {
        let path = case_dir("snapshot").join("in.srt");
        // 第二条字幕两行文本：验证多行原文的引用渲染
        std::fs::write(
            &path,
            "\
1
00:00:01,000 --> 00:00:03,000
山有木兮木有枝。

2
00:00:04,000 --> 00:00:06,000
心悦君兮
君不知。
",
        )
        .unwrap();
        let units = units_of(&path);
        assert_eq!(units[1].text, "心悦君兮\n君不知。");
        let translated: Vec<(String, String)> = units
            .iter()
            .map(|u| (u.id.clone(), format!("译-{}", u.text)))
            .collect();
        let md = to_appendix_markdown(&to_appendix_pages(&units, &translated));
        assert_eq!(
            md,
            "# 译文附录\n\n\
             ## 第 00:00:01,000 --> 00:00:03,000 页\n\n\
             > 山有木兮木有枝。\n\
             译-山有木兮木有枝。\n\n\
             ## 第 00:00:04,000 --> 00:00:06,000 页\n\n\
             > 心悦君兮\n\
             > 君不知。\n\
             译-心悦君兮\n君不知。"
        );
    }

    /// 空输入 / 全未命中 → 空 Vec / 空串；无条目页在 markdown 中跳过。
    #[test]
    fn empty_and_all_miss_yield_empty() {
        assert!(to_appendix_pages(&[], &[("p0-b0".into(), "译".into())]).is_empty());
        assert_eq!(to_appendix_markdown(&[]), "");

        let path = case_dir("empty").join("in.srt");
        std::fs::write(&path, SRT).unwrap();
        let units = units_of(&path);
        let miss = vec![("p9-b9".to_string(), "全错锚".to_string())];
        let pages = to_appendix_pages(&units, &miss);
        assert!(pages.is_empty(), "全未命中：无页组");
        assert_eq!(to_appendix_markdown(&pages), "");

        // 无条目页（构造不出来自 to_appendix_pages 的退化输入）不产悬空标题
        let blank_page = vec![AppendixPage {
            source_page: Some("p".into()),
            entries: Vec::new(),
        }];
        assert_eq!(to_appendix_markdown(&blank_page), "");
    }

    /// 单页件（txt）分组退化形态：多单元同页 → 一个页组、组内文档序；
    /// 空白译文忽略；重复 id 后者胜（胜者空白 = 未提供）。
    #[test]
    fn single_page_degenerate_and_latter_wins() {
        let path = case_dir("degenerate").join("in.txt");
        std::fs::write(&path, "第一段原文。\n\n第二段原文。\n\n第三段原文。\n").unwrap();
        let units = units_of(&path);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].page.as_deref(), Some("0"), "无名页 → 页序兜底");

        let translated = vec![
            (units[0].id.clone(), "前者。".to_string()),
            (units[0].id.clone(), "后者。".to_string()), // 重复 id：后者胜
            (units[1].id.clone(), "   \n\t ".to_string()), // 空白译文：忽略
            (units[2].id.clone(), "先到译。".to_string()),
            (units[2].id.clone(), "   ".to_string()), // 胜者空白 → 未提供
        ];
        let pages = to_appendix_pages(&units, &translated);

        assert_eq!(pages.len(), 1, "同页单元合入一个页组");
        assert_eq!(pages[0].source_page.as_deref(), Some("0"));
        assert_eq!(
            pages[0]
                .entries
                .iter()
                .map(|e| (e.unit_id.as_str(), e.translation.as_str()))
                .collect::<Vec<_>>(),
            [("p0-b0", "后者。")],
            "仅 id0 收录（后者胜）；id1 空白忽略；id2 胜者空白即未提供"
        );
    }

    /// markdown 的 None 页名分支（`## （无页名）`）与多行引用：直接构造
    /// 附录页（本模块自有类型，字段稳定）。
    #[test]
    fn markdown_none_page_branch() {
        let pages = vec![AppendixPage {
            source_page: None,
            entries: vec![AppendixEntry {
                unit_id: "x-b0".to_string(),
                original: "原\n文".to_string(),
                translation: "译".to_string(),
            }],
        }];
        assert_eq!(
            to_appendix_markdown(&pages),
            "# 译文附录\n\n## （无页名）\n\n> 原\n> 文\n译"
        );
    }
}
