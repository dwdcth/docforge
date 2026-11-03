//! TXT 章节切分（零依赖）。
//!
//! 算法骨架吸收自 document-parser 0.1.0-alpha.18 `utils/text.rs` 的
//! split_into_chapters 设计（见 THIRD_PARTY_NOTICES.md），词表按中文场景重写：
//! - 强类型（直接确认）：`第[数字/汉字数]+[章回节卷幕篇部]`、
//!   序章/序言/序幕/楔子/引子/番外/后记/尾声/终章/附录/前言、`Chapter N`；
//! - 弱类型（需相邻同类确认）：罗马数字 `IV.`、阿拉伯编号 `1.`——技术文档
//!   与列表里太常见，单个出现不作边界；
//! - 防误判：确认边界 <2 且无强类型 → 整篇不分章；首边界出现在正文 60%
//!   之后 → 视为误判放弃（正文里偶然的编号行）。
//!
//! TXT 没有可靠结构，任何短行都不该被激进地认作标题——宁可不切。

/// 一个确认的章边界：标题文本 + 行索引（0 起）。
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterBoundary {
    pub title: String,
    pub line_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadingKind {
    /// 明确关键词（第X章 / 楔子 / Chapter N）：自身即可靠
    Explicit,
    /// 部/卷级大结构：可靠，且与编号体系兼容
    Part,
    /// 罗马数字编号：弱
    Roman,
    /// 阿拉伯编号：弱
    Numbered,
}

/// 标题行长度上限（字符数）：超过则更可能是正文而非标题。
const MAX_TITLE_CHARS: usize = 40;
/// 首边界允许的最晚位置（占全文行数比例）：晚于此视为误判。
const FIRST_BOUNDARY_MAX_RATIO: f64 = 0.6;

/// 把 TXT 全文切分为章节。返回 `(标题, 正文行区间)` 列表；无确认边界时
/// 返回单个 `(None, 全文)`。
///
/// 返回的正文区间为行的迭代边界（`lines[start..end]`），标题行不在正文中。
pub fn split_chapters(text: &str) -> Vec<(Option<String>, std::ops::Range<usize>)> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let boundaries = confirmed_boundaries(&lines);
    if boundaries.is_empty() {
        return vec![(None, 0..lines.len())];
    }
    let mut out = Vec::with_capacity(boundaries.len() + 1);
    // 首个边界之前的正文（若非空）
    if boundaries[0].line_index > 0 {
        out.push((None, 0..boundaries[0].line_index));
    }
    for (i, b) in boundaries.iter().enumerate() {
        let start = b.line_index + 1; // 跳过标题行
        let end = boundaries.get(i + 1).map(|n| n.line_index).unwrap_or(lines.len());
        out.push((Some(b.title.clone()), start..end));
    }
    // 空正文章丢弃；全部为空（如只有标题行）时退回整篇不分章
    out.retain(|(_, range)| range.start < range.end);
    if out.is_empty() {
        return vec![(None, 0..lines.len())];
    }
    out
}

/// 候选行分类：返回 (规范化标题, 类型)。
fn classify_line(raw: &str) -> Option<(String, HeadingKind)> {
    let s = raw.trim().trim_matches('\u{FEFF}').trim();
    if s.is_empty() || s.chars().count() > MAX_TITLE_CHARS {
        return None;
    }
    // 中文特殊词（序言类）：整行即标题
    const SPECIALS: [&str; 11] = [
        "序章", "序言", "序幕", "楔子", "引子", "番外", "后记", "尾声", "终章", "附录", "前言",
    ];
    let head: String = s.chars().take(2).collect();
    if SPECIALS.iter().any(|w| s.starts_with(w)) || SPECIALS.iter().any(|w| head == *w) {
        return Some((s.to_string(), HeadingKind::Explicit));
    }
    // 第X章/回/节/卷/幕/篇/部：`第` + 1..=12 个数字字符 + 结构字
    if let Some(rest) = s.strip_prefix('第') {
        let nums: String = rest
            .chars()
            .take_while(|c| {
                c.is_ascii_digit() || "零一二三四五六七八九十百千万亿两〇".contains(*c)
            })
            .collect();
        let after = &rest[nums.len()..];
        let struct_char = after.chars().next();
        let is_part = matches!(struct_char, Some('部'));
        if !nums.is_empty()
            && matches!(struct_char, Some('章' | '回' | '节' | '卷' | '幕' | '篇' | '部'))
        {
            return Some((
                s.to_string(),
                if is_part { HeadingKind::Part } else { HeadingKind::Explicit },
            ));
        }
    }
    // Chapter N / CHAPTER N / chapter n
    let lower = s.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("chapter ") {
        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return Some((s.to_string(), HeadingKind::Explicit));
        }
    }
    // 罗马 IV. / IV、
    if let Some(rest) = s.strip_suffix(['.', '、', ' ', '．']) {
        if !rest.is_empty()
            && rest.len() <= 8
            && rest.chars().all(|c| "IVXLCDM".contains(c))
        {
            return Some((s.to_string(), HeadingKind::Roman));
        }
    }
    // 阿拉伯 1. / 1、/ 1 标题
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        let after: String = s.chars().skip(digits.len()).take(2).collect();
        if after.starts_with('.') || after.starts_with('、') || after.starts_with('．') {
            let title_tail = s.chars().skip(digits.len()).collect::<String>();
            // "12." 裸编号也接受为候选（弱类型需邻居确认）
            let _ = title_tail;
            return Some((s.to_string(), HeadingKind::Numbered));
        }
    }
    None
}

/// 全部候选 → 确认边界（应用强弱与位置规则）。
fn confirmed_boundaries(lines: &[&str]) -> Vec<ChapterBoundary> {
    let candidates: Vec<(usize, String, HeadingKind)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| classify_line(l).map(|(t, k)| (i, t, k)))
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    // 弱类型需相邻候选（前或后）为兼容类型才确认；强类型直接确认。
    // 只看后继会让链尾编号（"2. 续章"）永远无法确认，故前后都看。
    let compatible = |a: HeadingKind, b: HeadingKind| match (a, b) {
        (HeadingKind::Roman, HeadingKind::Roman) => true,
        (HeadingKind::Numbered, HeadingKind::Numbered) => true,
        // Part 与任何编号体系兼容（"第一部" + 后续 "第一章" 是常见结构）
        (HeadingKind::Part, _) | (_, HeadingKind::Part) => true,
        (HeadingKind::Explicit, HeadingKind::Explicit) => true,
        _ => false,
    };
    let mut confirmed: Vec<(usize, String, HeadingKind)> = Vec::new();
    for (idx, (i, t, k)) in candidates.iter().enumerate() {
        let strong = matches!(k, HeadingKind::Explicit | HeadingKind::Part);
        let prev_ok = idx > 0
            && candidates
                .get(idx - 1)
                .map(|(_, _, pk)| compatible(*k, *pk))
                .unwrap_or(false);
        let next_ok = candidates
            .get(idx + 1)
            .map(|(_, _, nk)| compatible(*k, *nk))
            .unwrap_or(false);
        if strong || prev_ok || next_ok {
            confirmed.push((*i, t.clone(), *k));
        }
    }
    // 确认数 <2 且无强类型：单凭弱类型不足以断言文档有章结构
    let has_strong = confirmed
        .iter()
        .any(|(_, _, k)| matches!(k, HeadingKind::Explicit | HeadingKind::Part));
    if confirmed.len() < 2 && !has_strong {
        return Vec::new();
    }
    // 首边界位置过晚：正文里偶然出现的编号/关键词，放弃分章
    let total = lines.len() as f64;
    if !confirmed.is_empty() && (confirmed[0].0 as f64) / total > FIRST_BOUNDARY_MAX_RATIO {
        return Vec::new();
    }
    confirmed
        .into_iter()
        .map(|(i, t, _)| ChapterBoundary { title: t, line_index: i })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapters(text: &str) -> Vec<(Option<String>, String)> {
        let lines: Vec<&str> = text.lines().collect();
        split_chapters(text)
            .into_iter()
            .map(|(t, r)| {
                let body = lines
                    .get(r.start..r.end)
                    .map(|ls| ls.join("\n"))
                    .unwrap_or_default();
                (t, body)
            })
            .collect()
    }

    #[test]
    fn explicit_chinese_chapter_detected() {
        let text = "开头引子几句话。\n\n第一章 初入江湖\n\n少年出门。\n\n第二章 风起\n\n夜里下雨。";
        let ch = chapters(text);
        assert_eq!(ch.len(), 3, "正文前段 + 两章：{ch:?}");
        assert_eq!(ch[1].0.as_deref(), Some("第一章 初入江湖"));
        assert!(ch[1].1.contains("少年出门"));
        assert_eq!(ch[2].0.as_deref(), Some("第二章 风起"));
    }

    #[test]
    fn weak_numbered_needs_neighbor() {
        // 单个编号行：不分章
        let single = "正文一段。\n\n1. 某个条目\n\n更多正文。";
        assert_eq!(chapters(single).len(), 1);

        // 两个相邻编号：确认分章
        let pair = "前言内容。\n\n1. 起点\n\n正文甲。\n\n2. 续章\n\n正文乙。";
        let ch = chapters(pair);
        assert_eq!(ch.len(), 3, "{ch:?}");
        assert_eq!(ch[1].0.as_deref(), Some("1. 起点"));
    }

    #[test]
    fn late_first_boundary_abandoned() {
        // 前面 10 行正文，第 11 行才出现"第三章"（>60%）——放弃
        let mut text = String::new();
        for i in 0..10 {
            text.push_str(&format!("正文第{i}行，叙述性长句占位内容。\n"));
        }
        text.push_str("第三章 突然出现的标题\n");
        text.push_str("尾部正文。\n");
        assert_eq!(chapters(&text).len(), 1, "首边界过晚应放弃分章");
    }

    #[test]
    fn special_headings() {
        let text = "楔子\n\n很多年前。\n\n第一章 开始\n\n正文。\n\n番外 小记\n\n附记内容。";
        let ch = chapters(text);
        let titles: Vec<_> = ch.iter().map(|(t, _)| t.clone()).collect();
        assert!(titles.contains(&Some("楔子".to_string())), "{titles:?}");
        assert!(titles.contains(&Some("番外 小记".to_string())), "{titles:?}");
    }

    #[test]
    fn no_chapters_single_flow() {
        let text = "这是普通段落一。\n\n这是普通段落二，没有章节结构。\n\n继续正文。";
        let ch = chapters(text);
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].0, None);
    }

    #[test]
    fn part_compat_with_numbered() {
        let text = "上部说明。\n\n第一部 风云\n\n甲正文。\n\n1. 小节\n\n乙正文。";
        let ch = chapters(text);
        assert!(ch.len() >= 2, "Part 应与编号体系兼容：{ch:?}");
    }

    #[test]
    fn chapter_number_forms() {
        // 汉字数字、阿拉伯数字都认
        for line in ["第十二章 高潮", "第3回 转折", "第一百零五节"] {
            assert_eq!(
                classify_line(line).map(|(t, _)| t),
                Some(line.to_string()),
                "{line}"
            );
        }
        // 正文长行不是标题
        assert!(classify_line("这句话很长，虽然以第字开头第三个人走了，但它是正文而不是标题行。").is_none());
        assert!(classify_line("第二天他去了市场买菜回家做饭。").is_none());
    }
}
