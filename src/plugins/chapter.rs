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
//! 小节层级（章内结构增强）：章内再识别三类小节标题，产 level 2/3 结构
//! 标记——`1.1` / `1.1.1` 阿拉伯点分层级（2 层 → level 2，3 层 → level 3）、
//! `一、` 中文数字 + 顿号（level 2）、`（一）` 括号中文数字（level 3）。
//! 小节是**弱类型**且**不是章边界**：不切段，只作为 body 内的行级标注
//! （`Section::subheadings`）随章返回。弱类型的确认依据：
//! - 技术文档与列表里 `1.1`、`一、` 太常见（步骤列举、时间线、小数/版本号
//!   等正文形态），单个小节行不足以断言结构——沿用骨架的弱类型语义，
//!   须相邻候选兼容确认（同类互证；`一、` 与 `（一）` 是中文文档经典的
//!   大小节配对，互为兼容）；
//! - 防误判加固：编号后必须跟非空标题文本（"1.1" 裸编号、"（一）" 裸括号
//!   都不作小节）；点分层级 ≥2 才是小节，单数字点号 `1.` 仍是章级 Numbered。
//!
//! TXT 没有可靠结构，任何短行都不该被激进地认作标题——宁可不切。
//!
//! 章节规则可配置化：[`ChapterRules`] 允许调用方传入自定义标题规则——
//! `extra` 规则**补充**到内置规则之后一并参与判定（`replace_builtin = false`，
//! 默认；内置先匹配，未命中的行走自定义），或 `replace_builtin = true` 时
//! **完全替代**内置规则（"第X章"/编号体系全部停用，只认自定义）。自定义
//! 规则是零依赖形态（行首前缀匹配，不用正则）：level 1 按**强类型**（直接
//! 确认）参与章边界；level 2/3 按小节处理（进 `subheadings`，弱类型确认
//! 规则同内置小节——自定义候选之间同类互证）。[`split_chapters`] 保持旧
//! 签名（内置规则），[`split_chapters_with`] 是主实现。

/// 一个确认的章边界：标题文本 + 行索引（0 起）。
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterBoundary {
    pub title: String,
    pub line_index: usize,
}

/// 标题：文本 + 层级（1 = 章级边界；2/3 = 章内小节）。
#[derive(Debug, Clone, PartialEq)]
pub struct Heading {
    pub title: String,
    pub level: u8,
}

/// 章内小节标记：不切段，只作 body 内的行级结构标注。
/// `line_index` 为全文行号（与 [`Section::body`] 同一坐标系）。
#[derive(Debug, Clone, PartialEq)]
pub struct Subheading {
    pub line_index: usize,
    pub level: u8,
    pub title: String,
}

/// 切分结果单元：章标题（`None` = 首个边界前的前言 / 无章文档的整篇）+
/// 正文行区间 + 章内小节标记。正文区间为行的迭代边界
/// （`lines[start..end]`）；章标题行不在区间内，小节行仍在区间内——
/// 由 `subheadings` 标注行号，消费方（text.rs 的 parse）自行剥离。
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub heading: Option<Heading>,
    pub body: std::ops::Range<usize>,
    pub subheadings: Vec<Subheading>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadingKind {
    /// 明确关键词（第X章 / 楔子 / Chapter N）：自身即可靠
    Explicit,
    /// 部/卷级大结构：可靠，且与编号体系兼容
    Part,
    /// 罗马数字编号：弱
    Roman,
    /// 阿拉伯编号（单层点号 `1.`）：弱
    Numbered,
}

/// 小节编号类型：弱类型确认时按"同类互证"分组。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubKind {
    /// `1.1` / `1.1.1` 阿拉伯点分层级
    Dotted,
    /// `一、` 中文数字 + 顿号
    Dun,
    /// `（一）` 括号中文数字
    Paren,
    /// 自定义规则（[`ChapterRules::extra`]）产出的候选：自成一类，
    /// 同类互证（自定义候选之间相邻兼容才确认，弱类型语义同内置小节）
    Custom,
}

/// 自定义标题规则（零依赖形态：前缀/关键词匹配，不用正则）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomRule {
    /// 行首前缀（trim 后），如 "Chapter "/"第 "；匹配要求前缀后仍有非空标题文本
    pub prefix: String,
    /// 产出层级（1=章级边界，2/3=节级标记）
    pub level: u8,
}

/// 章节切分规则配置：内置规则 + 传入规则覆盖或补充。
///
/// - 默认（[`ChapterRules::default`]）：`extra` 空、`replace_builtin` false，
///   行为与纯内置规则完全一致（[`split_chapters`] 即此形态）；
/// - **补充**（`replace_builtin: false`）：`extra` 追加到内置规则之后一并
///   参与判定；
/// - **覆盖**（`replace_builtin: true`）：只用 `extra`，内置规则全部停用
///   （"第X章" / 楔子 / 编号体系都不再分章）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterRules {
    /// 补充：追加到内置规则之后一并参与判定
    pub extra: Vec<CustomRule>,
    /// 覆盖：true 时只用 extra（内置全部停用）
    pub replace_builtin: bool,
}

impl Default for ChapterRules {
    /// 默认：无自定义规则、不覆盖内置（纯内置行为）。
    fn default() -> Self {
        Self {
            extra: Vec::new(),
            replace_builtin: false,
        }
    }
}

/// 标题行长度上限（字符数）：超过则更可能是正文而非标题。
const MAX_TITLE_CHARS: usize = 40;
/// 首边界允许的最晚位置（占全文行数比例）：晚于此视为误判。
const FIRST_BOUNDARY_MAX_RATIO: f64 = 0.6;
/// 小节用中文数字字符集（1-3 位组合，如 十一 / 二十三）。
const CHINESE_NUMERALS: &str = "一二三四五六七八九十";

/// 把 TXT 切分为章级 [`Section`] 列表：章边界切段（heading = level 1），
/// 章内小节（level 2/3）不切段，作为 `subheadings` 行级标注随章返回；
/// 无确认边界时返回单个 `heading = None` 的全文 Section。
///
/// 内置规则的便捷入口（等价于 [`split_chapters_with`] + [`ChapterRules::default`]）。
pub fn split_chapters(text: &str) -> Vec<Section> {
    split_chapters_with(text, &ChapterRules::default())
}

/// 按传入规则切分（主实现）：`rules` 补充或覆盖内置标题规则，见
/// [`ChapterRules`]。切分语义与 [`split_chapters`] 相同——章边界切段，
/// 章内小节作行级标注随章返回。
pub fn split_chapters_with(text: &str, rules: &ChapterRules) -> Vec<Section> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let boundaries = confirmed_boundaries(&lines, rules);
    if boundaries.is_empty() {
        return vec![Section {
            heading: None,
            body: 0..lines.len(),
            subheadings: confirmed_subheadings(&lines, 0, rules),
        }];
    }
    let mut out: Vec<Section> = Vec::with_capacity(boundaries.len() + 1);
    // 首个边界之前的正文（若非空）
    if boundaries[0].line_index > 0 {
        let body = 0..boundaries[0].line_index;
        let subs = confirmed_subheadings(&lines[body.clone()], 0, rules);
        out.push(Section { heading: None, body, subheadings: subs });
    }
    for (i, b) in boundaries.iter().enumerate() {
        let start = b.line_index + 1; // 跳过标题行
        let end = boundaries.get(i + 1).map(|n| n.line_index).unwrap_or(lines.len());
        if start >= end {
            continue; // 空正文章丢弃
        }
        let subs = confirmed_subheadings(&lines[start..end], start, rules);
        out.push(Section {
            heading: Some(Heading { title: b.title.clone(), level: 1 }),
            body: start..end,
            subheadings: subs,
        });
    }
    // 全部为空（如只有标题行）时退回整篇不分章
    if out.is_empty() {
        return vec![Section {
            heading: None,
            body: 0..lines.len(),
            subheadings: confirmed_subheadings(&lines, 0, rules),
        }];
    }
    out
}

/// 标题行预处理：去首尾空白与 BOM；空行或超长（> [`MAX_TITLE_CHARS`]）→ None。
fn normalize_title_line(raw: &str) -> Option<&str> {
    let s = raw.trim().trim_matches('\u{FEFF}').trim();
    if s.is_empty() || s.chars().count() > MAX_TITLE_CHARS {
        None
    } else {
        Some(s)
    }
}

/// 自定义规则匹配：行（已 trim 规范化）以 `rule.prefix` 开头，且前缀后
/// 仍有非空标题文本（裸前缀行如 "Chapter " 不作标题）。空前缀会命中一切
/// 标题行，视为配置错误、不参与判定。
fn match_custom_rule(s: &str, rule: &CustomRule) -> bool {
    !rule.prefix.is_empty()
        && s.strip_prefix(rule.prefix.as_str())
            .is_some_and(|rest| !rest.trim().is_empty())
}

/// 候选行分类：返回 (规范化标题, 类型)。
///
/// 规则来源按序：内置规则（`replace_builtin` 时全部跳过）→ 自定义 level 1
/// 规则（按**强类型**处理：自身即可靠、直接确认，走 [`HeadingKind::Explicit`]
/// 强类型通道，无需相邻同类互证）。
fn classify_line(raw: &str, rules: &ChapterRules) -> Option<(String, HeadingKind)> {
    let s = normalize_title_line(raw)?;
    if !rules.replace_builtin {
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
        // 阿拉伯 1. / 1、/ 1．（章级弱候选）。分隔符后仍是数字（"1.1"）说明是
        // 点分层级 ≥2 的小节编号，归小节体系（classify_subsection），不在此列。
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && digits.len() <= 3 {
            let sep = s.chars().nth(digits.len());
            if matches!(sep, Some('.') | Some('、') | Some('．')) {
                let after_sep = s.chars().nth(digits.len() + 1);
                if !after_sep.is_some_and(|c| c.is_ascii_digit()) {
                    return Some((s.to_string(), HeadingKind::Numbered));
                }
            }
        }
    }
    // 自定义章级规则（level 1）
    for rule in &rules.extra {
        if rule.level == 1 && match_custom_rule(&s, rule) {
            return Some((s.to_string(), HeadingKind::Explicit));
        }
    }
    None
}

/// 小节候选分类（弱类型）：返回 (规范化标题, 层级, 类型)。
/// 只认"编号 + 非空标题文本"的行——裸编号（"1.1" / "（一）"）不作小节。
///
/// 规则来源按序同 [`classify_line`]：内置小节体系（`replace_builtin` 时全跳）
/// → 自定义 level 2/3 规则（进小节体系，弱类型确认规则同内置小节：
/// [`SubKind::Custom`] 自定义候选之间同类互证）。level 1 归章边界、2/3 之外
/// 的层级未定义、不参与（防手滑配出 level 7 之类）。
fn classify_subsection(raw: &str, rules: &ChapterRules) -> Option<(String, u8, SubKind)> {
    let s = normalize_title_line(raw)?;
    if !rules.replace_builtin {
        // 1) 阿拉伯点分层级 `1.1` / `1.1.1`：每层 1-3 位数字、最多 3 层，
        //    点分层级即标题级别（2 层 → level 2，3 层 → level 3）
        if let Some((level, rest)) = dotted_depth(s) {
            let title = rest.trim_start_matches([' ', '\u{3000}']).trim();
            if !title.is_empty() {
                return Some((s.to_string(), level, SubKind::Dotted));
            }
            return None;
        }
        // 2) 中文数字（1-3 位）+ 顿号 `一、标题` → level 2
        let numeral: String = s.chars().take_while(|c| CHINESE_NUMERALS.contains(*c)).collect();
        let n = numeral.chars().count();
        if (1..=3).contains(&n) {
            if let Some(title) = s[numeral.len()..].strip_prefix('、') {
                if !title.trim().is_empty() {
                    return Some((s.to_string(), 2, SubKind::Dun));
                }
            }
        }
        // 3) 括号中文数字 `（一）标题` → level 3（全角为主，兼容半角括号）
        for (open, close) in [('（', '）'), ('(', ')')] {
            if let Some(rest) = s.strip_prefix(open) {
                let inner: String =
                    rest.chars().take_while(|c| CHINESE_NUMERALS.contains(*c)).collect();
                let m = inner.chars().count();
                if (1..=3).contains(&m) {
                    if let Some(title) = rest[inner.len()..].strip_prefix(close) {
                        if !title.trim().is_empty() {
                            return Some((s.to_string(), 3, SubKind::Paren));
                        }
                    }
                }
            }
        }
    }
    // 自定义小节规则（level 2/3）
    for rule in &rules.extra {
        if matches!(rule.level, 2..=3) && match_custom_rule(&s, rule) {
            return Some((s.to_string(), rule.level, SubKind::Custom));
        }
    }
    None
}

/// 解析行首点分数字编号：返回 (层级, 编号后的剩余文本)。
/// 每层 1-3 位数字、最多 3 层；层级 <2（单层点号 `1.`）返回 None——
/// 那是章级 Numbered 的领地，不算点分小节。
fn dotted_depth(s: &str) -> Option<(u8, &str)> {
    let bytes = s.as_bytes();
    let mut depth = 0u8;
    let mut idx = 0usize;
    loop {
        let start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() && idx - start < 3 {
            idx += 1;
        }
        if idx == start {
            return None; // 这一层没有数字
        }
        depth += 1;
        if depth > 3 {
            return None; // 超过 3 层
        }
        if idx >= bytes.len() {
            return None; // 编号到行尾：裸编号（"1.1"）无标题文本
        }
        if bytes[idx] != b'.' {
            break; // 点分编号结束
        }
        idx += 1;
    }
    if idx < bytes.len() && bytes[idx].is_ascii_digit() {
        return None; // 某层数字超 3 位（"1.1234"）：更像小数/版本号，放弃
    }
    if depth < 2 {
        return None; // 点分层级 ≥2 才是 Subsection
    }
    Some((depth, &s[idx..]))
}

/// 全部候选 → 确认边界（应用强弱与位置规则）。`rules` 传给候选分类。
fn confirmed_boundaries(lines: &[&str], rules: &ChapterRules) -> Vec<ChapterBoundary> {
    let candidates: Vec<(usize, String, HeadingKind)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| classify_line(l, rules).map(|(t, k)| (i, t, k)))
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

/// 章内小节扫描：候选 → 相邻兼容确认（弱类型语义，见模块注释）。
/// `base` 为 `lines[0]` 在全文中的行号，返回值换算回全文坐标系。
/// 只看前/后候选的兼容性（前后都看，链尾编号同样能确认），无全局阈值——
/// 小节标记不切段，误判代价远小于章边界，无需"确认数 <2"的整篇放弃规则。
/// `rules` 传给候选分类（自定义 level 2/3 规则的候选走 [`SubKind::Custom`]）。
fn confirmed_subheadings(lines: &[&str], base: usize, rules: &ChapterRules) -> Vec<Subheading> {
    let candidates: Vec<(usize, u8, SubKind)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| classify_subsection(l, rules).map(|(_, lv, k)| (i, lv, k)))
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    // 同类互证：点分与点分、顿号与顿号、括号与括号；顿号与括号是中文
    // 文档经典的大小节配对（"一、大节" 下挂 "（一）小节"），互为兼容。
    // 点分与中文数字体系不互证——两套编号并存时误判面更大。
    // 自定义候选（Custom）自成一类，同类互证。
    let compatible = |a: SubKind, b: SubKind| match (a, b) {
        (SubKind::Dotted, SubKind::Dotted)
        | (SubKind::Dun, SubKind::Dun)
        | (SubKind::Paren, SubKind::Paren)
        | (SubKind::Dun, SubKind::Paren)
        | (SubKind::Paren, SubKind::Dun)
        | (SubKind::Custom, SubKind::Custom) => true,
        _ => false,
    };
    let mut confirmed = Vec::new();
    for (idx, &(i, lv, k)) in candidates.iter().enumerate() {
        let prev_ok = idx > 0
            && candidates
                .get(idx - 1)
                .map(|(_, _, pk)| compatible(k, *pk))
                .unwrap_or(false);
        let next_ok = candidates
            .get(idx + 1)
            .map(|(_, _, nk)| compatible(k, *nk))
            .unwrap_or(false);
        if prev_ok || next_ok {
            confirmed.push(Subheading {
                line_index: base + i,
                level: lv,
                title: normalize_title_line(lines[i]).map(str::to_string).unwrap_or_default(),
            });
        }
    }
    confirmed
}

#[cfg(test)]
mod tests {
    use super::*;

    // 结构改为 Section 后适配：测试只关心 (章标题, 正文拼接)，断言语义不变。
    fn chapters(text: &str) -> Vec<(Option<String>, String)> {
        let lines: Vec<&str> = text.lines().collect();
        split_chapters(text)
            .into_iter()
            .map(|s| {
                let body = lines
                    .get(s.body.start..s.body.end)
                    .map(|ls| ls.join("\n"))
                    .unwrap_or_default();
                (s.heading.map(|h| h.title), body)
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
                classify_line(line, &ChapterRules::default()).map(|(t, _)| t),
                Some(line.to_string()),
                "{line}"
            );
        }
        // 正文长行不是标题
        assert!(classify_line(
            "这句话很长，虽然以第字开头第三个人走了，但它是正文而不是标题行。",
            &ChapterRules::default()
        )
        .is_none());
        assert!(classify_line("第二天他去了市场买菜回家做饭。", &ChapterRules::default()).is_none());
    }

    #[test]
    fn subsection_numeric_dotted() {
        let builtin = ChapterRules::default();
        assert_eq!(classify_subsection("1.1 概述", &builtin).map(|(_, lv, _)| lv), Some(2));
        assert_eq!(classify_subsection("1.2 安装", &builtin).map(|(_, lv, _)| lv), Some(2));
        assert_eq!(classify_subsection("1.1.1 背景", &builtin).map(|(_, lv, _)| lv), Some(3));
        // 4 层超限；单层点号是章级 Numbered，不作小节
        assert!(classify_subsection("1.1.1.1 太深", &builtin).is_none());
        assert!(classify_subsection("1. 单层点号", &builtin).is_none());
        // 点分层级 ≥2 不再是章级 Numbered 候选（Numbered/Subsection 的分界）
        assert_eq!(classify_line("1.1 概述", &builtin), None);
        assert_eq!(
            classify_line("1. 起点", &builtin).map(|(_, k)| k),
            Some(HeadingKind::Numbered)
        );
        // 小节不切段：无章文档整篇一个 Section，小节是行级标记
        let text = "1.1 概述\n\n说明文字。\n\n1.2 安装\n\n步骤内容。";
        let secs = split_chapters(text);
        assert_eq!(secs.len(), 1, "小节不是章边界：{secs:?}");
        assert!(secs[0].heading.is_none());
        assert_eq!(
            secs[0]
                .subheadings
                .iter()
                .map(|s| (s.line_index, s.level))
                .collect::<Vec<_>>(),
            vec![(0, 2), (4, 2)]
        );
    }

    #[test]
    fn chinese_ordinal_dun() {
        let builtin = ChapterRules::default();
        assert_eq!(classify_subsection("一、绪论", &builtin).map(|(_, lv, _)| lv), Some(2));
        assert_eq!(classify_subsection("二十三、附则", &builtin).map(|(_, lv, _)| lv), Some(2));
        // 首字不在数字集 / 数字后无顿号：不是小节
        assert!(classify_subsection("天人、合一", &builtin).is_none());
        assert!(classify_subsection("十一和弦", &builtin).is_none());
        // 小节不切段：整篇一个 Section，标记在 subheadings（全文行号）
        let text = "一、绪论\n\n内容甲。\n\n二、方法\n\n内容乙。";
        let secs = split_chapters(text);
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0].subheadings.len(), 2);
        assert_eq!(secs[0].subheadings[1].line_index, 4);
        assert_eq!(secs[0].subheadings[1].title, "二、方法");
    }

    #[test]
    fn parenthesized_chinese() {
        let builtin = ChapterRules::default();
        assert_eq!(classify_subsection("（一）研究背景", &builtin).map(|(_, lv, _)| lv), Some(3));
        assert_eq!(classify_subsection("（十二）研究方法", &builtin).map(|(_, lv, _)| lv), Some(3));
        // 裸括号编号（编号后无标题文本）不作小节
        assert!(classify_subsection("（一）", &builtin).is_none());
    }

    #[test]
    fn bare_number_not_subsection() {
        let builtin = ChapterRules::default();
        assert!(classify_subsection("1.1", &builtin).is_none());
        assert!(classify_subsection("1.1.1", &builtin).is_none());
        assert!(classify_subsection("1.1  ", &builtin).is_none());
        assert!(classify_subsection("一、", &builtin).is_none());
        // 弱类型语义：孤立小节候选无相邻同类佐证，不产出（防列表/版本号误判）
        let lone = "第一章 部署\n\n1.1 孤立小节\n\n正文内容。";
        let secs = split_chapters(lone);
        assert_eq!(secs.len(), 1);
        assert!(secs[0].subheadings.is_empty(), "{:?}", secs[0].subheadings);
    }

    // ── 章节规则可配置化（ChapterRules / CustomRule / split_chapters_with）──

    /// 规则版 chapters 助手：断言只关心 (章标题, 正文拼接)。
    fn chapters_with(text: &str, rules: &ChapterRules) -> Vec<(Option<String>, String)> {
        let lines: Vec<&str> = text.lines().collect();
        split_chapters_with(text, rules)
            .into_iter()
            .map(|s| {
                let body = lines
                    .get(s.body.start..s.body.end)
                    .map(|ls| ls.join("\n"))
                    .unwrap_or_default();
                (s.heading.map(|h| h.title), body)
            })
            .collect()
    }

    #[test]
    fn custom_prefix_rule_supplement() {
        // 补充模式（默认）：内置"第X章"与自定义 "Section " 前缀（level 1，
        // 强类型）混合件都分章
        let rules = ChapterRules {
            extra: vec![CustomRule { prefix: "Section ".to_string(), level: 1 }],
            replace_builtin: false,
        };
        let text = "第一章 出发\n\n清晨他离开村庄。\n\nSection 2 抵达\n\n他到达城镇。\n\n第三章 归来\n\n多年以后他回来了。";
        let ch = chapters_with(text, &rules);
        let titles: Vec<_> = ch.iter().map(|(t, _)| t.clone()).collect();
        assert_eq!(
            titles,
            vec![
                Some("第一章 出发".to_string()),
                Some("Section 2 抵达".to_string()),
                Some("第三章 归来".to_string()),
            ],
            "{titles:?}"
        );
        assert!(ch[1].1.contains("他到达城镇"));
        // 同一文件走默认规则：自定义行不是标题，只能两章
        let builtin = chapters_with(text, &ChapterRules::default());
        assert_eq!(builtin.len(), 2);
        assert!(builtin
            .iter()
            .all(|(t, _)| t.as_deref() != Some("Section 2 抵达")));
    }

    #[test]
    fn custom_rule_replace_builtin() {
        // 覆盖模式：replace_builtin = true 时内置"第X章"全部停用，只认自定义
        let rules = ChapterRules {
            extra: vec![CustomRule { prefix: "Part ".to_string(), level: 1 }],
            replace_builtin: true,
        };
        let text = "第一章 内置规则应失效\n\n正文一段。\n\nPart A 自定义甲\n\n内容甲。\n\nPart B 自定义乙\n\n内容乙。";
        let ch = chapters_with(text, &rules);
        let titles: Vec<_> = ch.iter().map(|(t, _)| t.clone()).collect();
        assert!(!titles.iter().any(|t| t.as_deref().is_some_and(|s| s.contains("第一章"))), "{titles:?}");
        // "第一章"行混在首个边界之前的正文里
        assert_eq!(ch[0].0, None);
        assert!(ch[0].1.contains("第一章 内置规则应失效"));
        assert_eq!(titles[1].as_deref(), Some("Part A 自定义甲"));
        assert_eq!(titles[2].as_deref(), Some("Part B 自定义乙"));
    }

    #[test]
    fn custom_rule_as_subsection() {
        // 自定义 level 2 规则进小节体系：不切段、进 subheadings，
        // 弱类型确认规则同内置小节（自定义候选之间同类互证）
        let rules = ChapterRules {
            extra: vec![CustomRule { prefix: "Step ".to_string(), level: 2 }],
            replace_builtin: false,
        };
        let text = "第一章 部署\n\nStep 1 准备\n\n安装依赖。\n\nStep 2 启动\n\n运行服务。";
        let secs = split_chapters_with(text, &rules);
        assert_eq!(secs.len(), 1, "小节不是章边界：{secs:?}");
        assert_eq!(
            secs[0].heading.as_ref().map(|h| h.title.as_str()),
            Some("第一章 部署")
        );
        assert_eq!(
            secs[0]
                .subheadings
                .iter()
                .map(|s| (s.line_index, s.level, s.title.as_str()))
                .collect::<Vec<_>>(),
            vec![(2, 2, "Step 1 准备"), (6, 2, "Step 2 启动")]
        );
        // 孤立自定义小节候选无相邻同类佐证：不产出（弱类型语义同内置）
        let lone = "第一章 部署\n\nStep 9 孤立小节\n\n正文内容。";
        let secs = split_chapters_with(lone, &rules);
        assert!(secs[0].subheadings.is_empty(), "{:?}", secs[0].subheadings);
    }

    #[test]
    fn builtin_behavior_unchanged_with_default_rules() {
        // 默认规则（extra 空、不覆盖内置）输出与改造前完全一致：
        // 旧入口 split_chapters 与 split_chapters_with(Default) 显式对照
        // （既有内置规则测试作回归兜底）。
        let text = "序章 引子\n\n开场内容。\n\n第一章 部署\n\n1.1 准备\n\n安装依赖。\n\n1.2 启动\n\n（一）检查\n\n第二章 验收\n\n尾部正文。";
        assert_eq!(split_chapters(text), split_chapters_with(text, &ChapterRules::default()));
        assert_eq!(
            split_chapters_with(text, &ChapterRules { extra: vec![], replace_builtin: false }),
            split_chapters_with(text, &ChapterRules::default())
        );
        // 该样例在内置规则下的形态：序章 + 两章边界，第一章带 1.1/1.2 小节
        let secs = split_chapters_with(text, &ChapterRules::default());
        let titles: Vec<_> = secs
            .iter()
            .map(|s| s.heading.as_ref().map(|h| h.title.clone()))
            .collect();
        assert_eq!(
            titles,
            vec![
                Some("序章 引子".to_string()),
                Some("第一章 部署".to_string()),
                Some("第二章 验收".to_string()),
            ],
            "{titles:?}"
        );
        assert_eq!(
            secs[1]
                .subheadings
                .iter()
                .map(|s| (s.line_index, s.level))
                .collect::<Vec<_>>(),
            vec![(6, 2), (10, 2)]
        );
    }
}
