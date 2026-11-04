// docforge 新增（翻译闭环①写侧）：TXT 双语回写。
//! TXT 双语回写：行级重扫描对齐 + 纯文本重组（非字节透传格式里唯一的零依赖
//! 通道，恒开无 feature 门控）。
//!
//! # 对齐口径（与 `plugins/text.rs` 提取侧逐条对应）
//!
//! * 提取侧管线：`TextParseCapability::default()`（章节切分 + 分节产块序，
//!   规则 = [`crate::plugins::chapter::ChapterRules::default`]）→
//!   `TextPlugin::assemble`（level → Heading，其余 Paragraph，单页）。txt 的
//!   块序 = item 序 = 解析序，重解析同一文件稳定（确定性解析）。回写侧
//!   **重跑同一管线**拿权威 id/文本（`p0-b{块序}`，页名 None → 页序 0），
//!   再以同一套切分/切段逻辑**重扫描原文行**，把每个块映回「块 × 行区间」：
//!   章标题 = 边界行（`body.start - 1`，切分器跳标题行的既定不变量）、
//!   小节 = `subheadings` 标注行、段落 = 缓冲行的首尾区间。
//! * 可翻译判据 = [`crate::emit::is_translatable`] + `min_chars = 2` +
//!   空白文本跳过（与 `to_translation_units` 完全同口径）。
//! * 双保险：每个定位到的块与权威单元文本做一致性比对，不一致跳过该块
//!   （宁漏勿错）；未匹配 id / 空白译文一律忽略；
//! * ①→④ 边界校验：[`crate::emit::TranslationPair::source`]（消费者提取
//!   译文依据的①侧源文本）与本波④单元文本不一致 → 该单元跳过并记报告
//!   （①提取与④回写之间文件被改时，位置锚点在新文件里指向别的文本，
//!   写下去就是静默错位写）；`None` = 信任锚点（不校验）。结果以
//!   [`crate::writeback::WritebackReport`] 返回：`written` = 实际写入单元
//!   数；`skipped` 只收「有译文但未写」（源文本不一致 / 文本守卫不一致；
//!   无译文 = 正常未翻译不记，编码拒绝类是 `Err` 不记）。
//!
//! # 三种策略（[`crate::emit::BiStrategy`]）
//!
//! * `BilingualInline`：原段**行后紧跟译文行**（`\n` 直连，对照紧凑，与 md
//!   回写同一形态）。**译文行一律纯文本**——txt 原文没有 `#` 标记，Heading
//!   译文不加任何前缀（加了反而会被重解析/阅读器当成正文符号）。重解析口径：
//!   txt 段落按空行切分，紧跟原段行的译文行重解析时与原段合成一段；章/节
//!   标题行是切分/剥离边界，其译文行独立成段——文件内对照形态与 md 双语
//!   一致，重解析的块结构差异是 txt 段模型的固有语义；
//! * `TranslationOnly`：原块行区间整体替换为译文单行（多行段落坍缩为一行，
//!   与 docx「首 `w:t` 换译文其余清空」的重解析等价口径一致）；
//! * `OriginalPlusAppendix`：原文完整不动，文尾追加空行 + `【译文】` 分隔行 +
//!   译文段（单元序，段间空行）。
//!
//! # 编码
//!
//! UTF-8 件（含 BOM）逐字节保真改写（未触及的行/换行原样保留，BOM 写回补回）；
//! 非 UTF-8 件走解析侧同一条解码链
//!（[`crate::encoding::detect_and_decode`]）：GBK/Big5/UTF-16 等命中即转存
//! UTF-8 写出（txt 无文件内编码声明，转存不破坏任何结构），全链未命中的
//! lossy 兜底件拒绝回写（宁漏勿错，避免把替换字符烤进输出）。

use std::collections::BTreeMap;
use std::path::Path;

use crate::emit::BiStrategy;
use crate::error::{DocError, Result};
use crate::plugins::chapter::{split_chapters_with, ChapterRules, Subheading};
use crate::plugins::text::{TextParseCapability, TextPlugin};
use crate::plugin::{FormatPlugin, ParseCapability};

use super::pair_map;

/// 附录分隔标记（OriginalPlusAppendix 文尾行）。
const APPENDIX_MARK: &str = "【译文】";

/// txt 双语回写：见模块级文档。`pairs` 的 id 与
/// [`crate::emit::to_translation_units`] 同口径（`p0-b{块序}`）；未匹配 /
/// 空白译文条目忽略；失败返回 `corrupt`。返回
/// [`crate::writeback::WritebackReport`]（口径见模块级文档）。
pub fn writeback_txt(
    in_path: impl AsRef<Path>,
    pairs: &[crate::emit::TranslationPair],
    strategy: BiStrategy,
    out: impl AsRef<Path>,
) -> Result<crate::writeback::WritebackReport> {
    let in_path = in_path.as_ref();
    let out = out.as_ref();

    // A. 读字节并解码（UTF-8 快路径逐字节保真；其余走解析侧同链）
    let bytes = std::fs::read(in_path)
        .map_err(|e| DocError::corrupt(format!("cannot read {}: {e}", in_path.display())))?;
    let (bom, text) = decode_input(&bytes)?;

    // B. 权威解析（提取侧同管线、同默认规则）拿 id / 文本
    let raw = TextParseCapability::default().parse(in_path)?;
    let doc = TextPlugin.assemble(&raw);
    let units: Vec<_> = crate::emit::to_translation_units(&doc)
        .into_iter()
        // 表格单元格单元（kind = Table）恒久不进 txt 回写：txt 无表格结构
        //（解析侧只产 Heading/Paragraph），过滤纯防御——单元流与块流同一
        // 口径，未来 txt 管线若有变化也不会错位。
        .filter(|u| u.kind != crate::model::BlockKind::Table)
        .collect();
    let map = pair_map(pairs);
    let mut report = crate::writeback::WritebackReport::default();

    // C. 重扫描原文行，产块 × 行区间（解析侧同口径）
    let spans = line_spans(&text);
    let slots = scan_slots(&text);
    if doc.pages.len() != 1 {
        return Err(DocError::corrupt("txt 对齐失败：Document 页数异常"));
    }
    let blocks = &doc.pages[0].blocks;
    if slots.len() != blocks.len() {
        return Err(DocError::corrupt("txt 对齐失败：重扫描槽位数与解析块数不一致"));
    }

    // D. 块 × 单元按序配对（同一过滤口径）+ 一致性守卫 + 源校验 + 译文查找
    let mut matched: Vec<(usize, String)> = Vec::new(); // (块序, 译文)，文档序
    let mut cursor = 0usize;
    for (k, b) in blocks.iter().enumerate() {
        let t = b.content.trim();
        if !crate::emit::is_translatable(b.kind) || t.is_empty() || t.chars().count() < 2 {
            continue;
        }
        let Some(unit) = units.get(cursor) else { break };
        cursor += 1;
        // 双保险：重扫描文本必须与权威单元文本一致（④ 内部守卫）；有译文
        // 的单元守卫失败记报告（无译文 = 正常未翻译，不是跳过）
        if slots[k].text != unit.text {
            if let Some((tr, _)) = map.get(unit.id.as_str()) {
                if !tr.trim().is_empty() {
                    report.skip(unit.id.clone(), "文本守卫不一致");
                }
            }
            continue;
        }
        let Some((tr, src_opt)) = map.get(unit.id.as_str()).copied() else {
            continue;
        };
        let tr = tr.trim();
        if tr.is_empty() {
            continue;
        }
        // ①→④ 边界：译文对携带的①源文本与当前(④)文本不一致 → 跳过并记报告
        //（位置锚点在新文件里指向别的文本，写下去就是静默错位写）
        if let Some(src) = src_opt {
            if src.trim() != unit.text.trim() {
                report.skip(unit.id.clone(), "源文本不一致（提取后文件已变更）");
                continue;
            }
        }
        matched.push((k, tr.to_string()));
        report.written += 1;
    }

    // E. 按策略重组（Appendix 原文原样 + 文尾附录；Inline/Only 行级重组）
    let out_text = match strategy {
        BiStrategy::OriginalPlusAppendix => {
            if matched.is_empty() {
                text
            } else {
                let mut s = text;
                s.push_str("\n\n");
                s.push_str(APPENDIX_MARK);
                s.push_str("\n\n");
                let trs: Vec<&str> = matched.iter().map(|(_, t)| t.as_str()).collect();
                s.push_str(&trs.join("\n\n"));
                s
            }
        }
        _ => rebuild(&text, &spans, &slots, &matched, strategy),
    };

    // F. 写出（UTF-8；BOM 补回）
    let mut out_bytes = Vec::with_capacity(out_text.len() + 3);
    if bom {
        out_bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    out_bytes.extend_from_slice(out_text.as_bytes());
    std::fs::write(out, out_bytes)
        .map_err(|e| DocError::corrupt(format!("cannot create {}: {e}", out.display())))?;
    Ok(report)
}

/// 输入解码：UTF-8（BOM 剥除后）直取；非 UTF-8 走解析侧同链
/// [`crate::encoding::detect_and_decode`]，lossy 兜底件拒绝（见模块级文档）。
fn decode_input(bytes: &[u8]) -> Result<(bool, String)> {
    let (bom, rest) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };
    if let Ok(s) = std::str::from_utf8(rest) {
        return Ok((bom, s.to_owned()));
    }
    let (text, name) = crate::encoding::detect_and_decode(rest);
    if name == "lossy-utf8" {
        return Err(DocError::corrupt(
            "txt 非 UTF-8 且解码链未命中（lossy 兜底）：回写拒绝，避免有损转存",
        ));
    }
    Ok((false, text)) // GBK/Big5/UTF-16 等命中：转存 UTF-8
}

/// 行字节区间：`(行起点, 行终点)`，终点不含换行符；切分与 `str::lines()`
/// 逐行同构（空文件 0 行；`\r\n` 的 `\r` 留在行内容里，重组时按原字节透传）。
fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push((start, i));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push((start, bytes.len()));
    }
    out
}

/// 一个块的重扫描定位：文本 + 层级 + 行区间（`end_line` 不含，块最后一行 = `end_line - 1`）。
struct TxtSlot {
    text: String,
    /// 块层级镜像（提取侧 level → Heading 的依据；对齐只用 text，字段留作
    /// 槽位自描述/调试）。
    #[allow(dead_code)]
    level: Option<u8>,
    start_line: usize,
    end_line: usize,
}

/// 块 × 行区间重扫描：[`split_chapters_with`]（默认规则）→ 与
/// `TextParseCapability::parse` 完全同款的切段循环，仅附带行区间。
/// 章标题行 = `body.start - 1`（切分器「跳标题行」的既定不变量）；
/// 小节行 = `subheadings` 的全文行号；段落 = 缓冲行首尾区间。
fn scan_slots(text: &str) -> Vec<TxtSlot> {
    let mut slots = Vec::new();
    for section in split_chapters_with(text, &ChapterRules::default()) {
        if let Some(h) = &section.heading {
            let hl = section.body.start.saturating_sub(1);
            slots.push(TxtSlot {
                text: h.title.clone(),
                level: Some(h.level),
                start_line: hl,
                end_line: hl + 1,
            });
        }
        // 章内小节（level 2/3）不切段：小节行从段落剥离，按行号回插
        let subs: BTreeMap<usize, &Subheading> = section
            .subheadings
            .iter()
            .map(|s| (s.line_index, s))
            .collect();
        let mut buf: Vec<(usize, &str)> = Vec::new();
        for (i, line) in text
            .lines()
            .enumerate()
            .skip(section.body.start)
            .take(section.body.end.saturating_sub(section.body.start))
        {
            if let Some(sub) = subs.get(&i) {
                flush(&mut buf, &mut slots);
                slots.push(TxtSlot {
                    text: sub.title.clone(),
                    level: Some(sub.level),
                    start_line: i,
                    end_line: i + 1,
                });
            } else if line.trim().is_empty() {
                flush(&mut buf, &mut slots);
            } else {
                buf.push((i, line));
            }
        }
        flush(&mut buf, &mut slots);
    }
    slots
}

/// 段缓冲冲刷为段落槽位（与 `text.rs::flush_paragraph` 的 join/trim/去空一致）。
fn flush(buf: &mut Vec<(usize, &str)>, slots: &mut Vec<TxtSlot>) {
    if buf.is_empty() {
        return;
    }
    let joined = buf
        .iter()
        .map(|(_, l)| *l)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    let start_line = buf[0].0;
    let end_line = buf[buf.len() - 1].0 + 1;
    buf.clear();
    if !joined.is_empty() {
        slots.push(TxtSlot {
            text: joined,
            level: None,
            start_line,
            end_line,
        });
    }
}

/// BilingualInline / TranslationOnly 的行级重组：
///
/// * Inline：块末行后插入 `\n + 译文行`（行间分隔字节原样透传 → 译文紧贴
///   原段、与下一段的分隔形态保持原样）；
/// * TranslationOnly：块行区间整体替换为译文单行——区间内部换行丢弃，
///   区间前后的分隔字节保留（空行结构不变）。
fn rebuild(
    text: &str,
    spans: &[(usize, usize)],
    slots: &[TxtSlot],
    matched: &[(usize, String)],
    strategy: BiStrategy,
) -> String {
    // Inline：末行行号 → 译文（同一行只会命中一个块——行区间互不重叠）
    let mut ins_after: BTreeMap<usize, &str> = BTreeMap::new();
    // Only：行区间替换（按起点排序；块区间互不重叠）
    let mut skips: Vec<(usize, usize, &str)> = Vec::new();
    for (k, tr) in matched {
        let slot = &slots[*k];
        match strategy {
            BiStrategy::BilingualInline => {
                ins_after.insert(slot.end_line - 1, tr.as_str());
            }
            BiStrategy::TranslationOnly => {
                skips.push((slot.start_line, slot.end_line, tr.as_str()));
            }
            BiStrategy::OriginalPlusAppendix => unreachable!("已在分支前提前返回"),
        }
    }
    skips.sort_by_key(|(s, _, _)| *s);

    let mut out = String::with_capacity(text.len());
    let mut skip_ptr = 0usize;
    for li in 0..spans.len() {
        let (s, e) = spans[li];
        // 推进已越过的替换区间（区间互不重叠，逐个消费）
        while skip_ptr < skips.len() && li >= skips[skip_ptr].1 {
            skip_ptr += 1;
        }
        let active = skip_ptr < skips.len() && li >= skips[skip_ptr].0;
        // 1. 行间分隔字节（含换行）：仅被替换区间内部（前行与本行同区间）丢弃
        if li > 0 && !(active && li > skips[skip_ptr].0) {
            out.push_str(&text[spans[li - 1].1..s]);
        }
        // 2. 行内容
        if active {
            if li == skips[skip_ptr].1 - 1 {
                out.push_str(skips[skip_ptr].2); // 区间末行：译文替换整段
                skip_ptr += 1;
            }
            continue;
        }
        out.push_str(&text[s..e]);
        if let Some(tr) = ins_after.get(&li) {
            out.push('\n');
            out.push_str(tr);
        }
    }
    // 尾部字节（末行换行符，如有）
    if let Some(&(_, e)) = spans.last() {
        out.push_str(&text[e..]);
    }
    out
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::emit::to_translation_units;
    use crate::model::BlockKind::{Heading, Paragraph};

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_txt_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 提取侧同管线（TextParseCapability 默认规则 + TextPlugin）。
    fn parse_doc(path: &Path) -> crate::model::Document {
        let raw = TextParseCapability::default()
            .parse(path)
            .unwrap();
        TextPlugin.assemble(&raw)
    }

    fn unit_ids(path: &Path) -> Vec<String> {
        to_translation_units(&parse_doc(path))
            .into_iter()
            .map(|u| u.id)
            .collect()
    }

    fn write(
        in_path: &Path,
        out_path: &Path,
        tr: &[(&str, &str)],
        strategy: BiStrategy,
    ) -> crate::writeback::WritebackReport {
        let pairs: Vec<crate::emit::TranslationPair> =
            tr.iter().map(|(a, b)| (a.to_string(), b.to_string()).into()).collect();
        writeback_txt(in_path, &pairs, strategy, out_path).unwrap()
    }

    const NOVEL: &str = "\
第一章 出发

清晨他离开村庄。

1.1 准备

安装依赖。

1.2 启动

运行服务。

第二章 归来

多年以后他回来了。

甲

尾段一个句子。
";

    /// 金测（核心验收）：BilingualInline 往返 —— 章/节标题与段落原/译交错
    /// （标题译文纯文本、无 `#` 前缀）、1 字符短块不占翻译位、未匹配 id 忽略。
    #[test]
    fn roundtrip_bilingual_inline() {
        let dir = case_dir("inline");
        let in_path = dir.join("in.txt");
        std::fs::write(&in_path, NOVEL).unwrap();

        // 提取侧 id 口径（甲 = 1 字符 < min_chars，不发锚点；标题也是候选）
        assert_eq!(
            unit_ids(&in_path),
            ["p0-b0", "p0-b1", "p0-b2", "p0-b3", "p0-b4", "p0-b5", "p0-b6", "p0-b7", "p0-b9"]
        );

        let out_path = dir.join("out.txt");
        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "Set out."), // 章标题译文：纯文本行，无 `#` 前缀
                ("p0-b1", "He left the village at dawn."),
                ("p0-b2", "Prepare."), // 小节标题译文同样纯文本
                ("p0-b3", "Install the dependencies."),
                ("p0-b4", "Launch."),
                ("p0-b5", "Run the service."),
                ("p0-b6", "Return."),
                ("p0-b7", "Many years later he came back."),
                ("p0-b9", "The final paragraph."),
                ("p0-b99", "bogus"), // 未匹配 id：忽略
            ],
            BiStrategy::BilingualInline,
        );

        // 重解析：原/译交错；标题仍为标题（层级不变），译文行一律段落
        let blocks = &parse_doc(&out_path).pages[0].blocks;
        // 重解析口径：txt 段落按空行切分，紧跟原段行的译文行与原段合成
        // 一段（与 md 回写「不空行同段对照」同一形态）；章标题行是切分边界、
        // 小节行从段落剥离，两者的译文行各自独立成段。
        let kinds: Vec<crate::model::BlockKind> = blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                Heading,   // 第一章 出发（切分边界，译文行不入标题块）
                Paragraph, // Set out.
                Paragraph, // 清晨… + 译文行（同段对照）
                Heading,   // 1.1 准备（小节行剥离）
                Paragraph, // Prepare.
                Paragraph, // 安装依赖 + 译文行（同段对照）
                Heading,   // 1.2 启动
                Paragraph, // Launch.
                Paragraph, // 运行服务 + 译文行（同段对照）
                Heading,   // 第二章 归来
                Paragraph, // Return.
                Paragraph, // 多年以后 + 译文行（同段对照）
                Paragraph, // 甲：不占翻译位，原样、无译邻行
                Paragraph, // 尾段 + 译文行（同段对照）
            ]
        );
        let contents: Vec<&str> = blocks.iter().map(|b| b.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "第一章 出发",
                "Set out.",
                "清晨他离开村庄。\nHe left the village at dawn.",
                "1.1 准备",
                "Prepare.",
                "安装依赖。\nInstall the dependencies.",
                "1.2 启动",
                "Launch.",
                "运行服务。\nRun the service.",
                "第二章 归来",
                "Return.",
                "多年以后他回来了。\nMany years later he came back.",
                "甲",
                "尾段一个句子。\nThe final paragraph.",
            ]
        );
        assert_eq!(blocks[0].level, Some(1));
        assert_eq!(blocks[3].level, Some(2), "小节标题不受译文行扰动");
        assert_eq!(blocks[6].level, Some(2));
        assert_eq!(blocks[9].level, Some(1));
        assert!(!std::fs::read_to_string(&out_path).unwrap().contains("bogus"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TranslationOnly：多行段落坍缩为译文单行（区间内换行丢弃、前后空行
    /// 结构保留）；未翻译块原样。
    #[test]
    fn translation_only_replaces_lines() {
        let dir = case_dir("tonly");
        let in_path = dir.join("in.txt");
        std::fs::write(
            &in_path,
            "\
第一章 起

第一段第一行，
第一段第二行。

乙

第二段完整句子。
",
        )
        .unwrap();
        assert_eq!(unit_ids(&in_path), ["p0-b0", "p0-b1", "p0-b3"]);

        let out_path = dir.join("out.txt");
        write(
            &in_path,
            &out_path,
            &[
                ("p0-b0", "The beginning."),
                ("p0-b1", "First paragraph in one line."),
                ("p0-b3", "Replaced second paragraph."),
            ],
            BiStrategy::TranslationOnly,
        );

        let blocks = &parse_doc(&out_path).pages[0].blocks;
        let contents: Vec<&str> = blocks.iter().map(|b| b.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "The beginning.", // 标题换译文（重解析后成普通段落——txt 无标题标记）
                "First paragraph in one line.", // 多行段落 → 译文单行
                "乙",            // 不占翻译位：原样
                "Replaced second paragraph.",
            ]
        );
        assert_eq!(blocks[0].kind, Paragraph);
        assert_eq!(blocks[2].kind, Paragraph);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：原文逐字节不动 + 文尾【译文】分隔 + 译文段；
    /// 全部未匹配时输出与输入逐字节一致（透传）。
    #[test]
    fn appendix_and_unmatched_passthrough() {
        let dir = case_dir("appendix");
        let in_path = dir.join("in.txt");
        std::fs::write(&in_path, NOVEL).unwrap();
        let out_path = dir.join("out.txt");
        write(
            &in_path,
            &out_path,
            &[
                ("p0-b1", "He left the village at dawn."),
                ("p0-b9", "The final paragraph."),
            ],
            BiStrategy::OriginalPlusAppendix,
        );

        let out = std::fs::read_to_string(&out_path).unwrap();
        assert!(out.starts_with(NOVEL), "原文完整前置");
        let marker = out.find(APPENDIX_MARK).unwrap();
        assert_eq!(
            &out[marker..],
            "【译文】\n\nHe left the village at dawn.\n\nThe final paragraph."
        );
        assert_eq!(marker, NOVEL.len() + 2, "原文与附录间空行分隔");

        // 全部未匹配：逐字节透传
        let out2 = dir.join("out2.txt");
        write(&in_path, &out2, &[("p0-b42", "nope")], BiStrategy::BilingualInline);
        assert_eq!(std::fs::read(&out2).unwrap(), NOVEL.as_bytes());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// UTF-8 BOM 剥除后扫描、写回补回；非 UTF-8：`encoding` feature 下
    /// GBK 命中转存 UTF-8，未开 feature 的 lossy 兜底件拒绝。
    #[test]
    fn bom_and_non_utf8_inputs() {
        let dir = case_dir("enc");

        // BOM：与解析侧 decode 剥 BOM 同口径，译文往返 + BOM 保留
        let bom_path = dir.join("bom.txt");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("第一章 起\n\n正文段落。\n".as_bytes());
        std::fs::write(&bom_path, &bytes).unwrap();
        let bom_out = dir.join("bom-out.txt");
        write(
            &bom_path,
            &bom_out,
            &[("p0-b0", "The beginning."), ("p0-b1", "Body text.")],
            BiStrategy::BilingualInline,
        );
        let raw = std::fs::read(&bom_out).unwrap();
        assert!(raw.starts_with(&[0xEF, 0xBB, 0xBF]), "BOM 补回");
        assert!(String::from_utf8_lossy(&raw).contains("第一章 起\nThe beginning."));

        // GBK 件：`encoding` feature 下解码链命中 GB18030 → 回写转存 UTF-8；
        // 未开 feature 的 lossy 兜底件拒绝回写。
        let bad_path = dir.join("gbk.txt");
        let bad_out = dir.join("gbk-out.txt");

        #[cfg(feature = "encoding")]
        {
            let src = "第一章 起\n\n中文正文段落。\n\n尾段一个句子。\n";
            let (gbk_bytes, _, had_errors) = encoding_rs::GB18030.encode(src);
            assert!(!had_errors);
            std::fs::write(&bad_path, &gbk_bytes).unwrap();
            // 解码链命中：回写成功，输出转存 UTF-8
            write(
                &bad_path,
                &bad_out,
                &[
                    ("p0-b0", "The beginning."),
                    ("p0-b1", "Chinese body paragraph."),
                    ("p0-b2", "The ending."),
                ],
                BiStrategy::BilingualInline,
            );
            let out = std::fs::read_to_string(&bad_out).unwrap();
            assert_eq!(
                out,
                "第一章 起\nThe beginning.\n\n中文正文段落。\nChinese body paragraph.\n\n尾段一个句子。\nThe ending.\n"
            );
        }
        #[cfg(not(feature = "encoding"))]
        {
            // 无法规可依的字节（非 UTF-8、无 BOM）→ lossy 兜底 → 拒绝回写
            std::fs::write(&bad_path, [0xD6u8, 0xD0, 0xCE, 0xC4, 0x0A, 0x0A, b'b']).unwrap();
            let err = writeback_txt(
                &bad_path,
                &[("p0-b0".to_string(), "Chinese.".to_string()).into()],
                BiStrategy::BilingualInline,
                &bad_out,
            )
            .unwrap_err();
            assert_eq!(err.code.as_str(), "corrupt");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 源校验（①→④ 边界）金测：`source` 与④文本一致 → 与二元组形态
    /// （source = None）逐字节等价（迁移无行为漂移）；`source` 不一致
    ///（构造：提取后改 pair.source 为别串）→ 该单元跳过（原处保持原文）
    /// + report.skipped 含 (id, "源文本不一致…")。
    #[test]
    fn source_mismatch_skips_and_reports() {
        let dir = case_dir("srcmismatch");
        let in_path = dir.join("in.txt");
        std::fs::write(&in_path, "第一章 起\n\n正文一段句子。\n\n尾段完整句子。\n").unwrap();
        assert_eq!(unit_ids(&in_path), ["p0-b0", "p0-b1", "p0-b2"]);

        // 中间单元的源文本故意与文件不一致（模拟①→④之间文件被改）
        let pairs: Vec<crate::emit::TranslationPair> = vec![
            (
                "p0-b0".to_string(),
                "第一章 起".to_string(),
                "The beginning.".to_string(),
            )
                .into(),
            (
                "p0-b1".to_string(),
                "提取后被改掉的别串。".to_string(),
                "Drifted body.".to_string(),
            )
                .into(),
            (
                "p0-b2".to_string(),
                "尾段完整句子。".to_string(),
                "The ending.".to_string(),
            )
                .into(),
        ];
        let out_path = dir.join("out.txt");
        let report =
            writeback_txt(&in_path, &pairs, BiStrategy::BilingualInline, &out_path).unwrap();
        assert_eq!(report.written, 2, "一致的两条写入");
        assert_eq!(
            report.skipped,
            vec![(
                "p0-b1".to_string(),
                "源文本不一致（提取后文件已变更）".to_string()
            )]
        );
        let out = std::fs::read_to_string(&out_path).unwrap();
        assert!(out.contains("正文一段句子。"), "源不一致的单元保持原文");
        assert!(!out.contains("Drifted body."), "错位译文不落盘");
        assert!(out.contains("The beginning.") && out.contains("The ending."));

        // 迁移等价性：二元组（source = None）与三元组一致源输出逐字节一致
        let tuples: Vec<crate::emit::TranslationPair> = vec![
            ("p0-b0".to_string(), "The beginning.".to_string()).into(),
            ("p0-b1".to_string(), "Body text.".to_string()).into(),
            ("p0-b2".to_string(), "The ending.".to_string()).into(),
        ];
        let with_src: Vec<crate::emit::TranslationPair> = vec![
            (
                "p0-b0".to_string(),
                "第一章 起".to_string(),
                "The beginning.".to_string(),
            )
                .into(),
            (
                "p0-b1".to_string(),
                "正文一段句子。".to_string(),
                "Body text.".to_string(),
            )
                .into(),
            (
                "p0-b2".to_string(),
                "尾段完整句子。".to_string(),
                "The ending.".to_string(),
            )
                .into(),
        ];
        let out_none = dir.join("out-none.txt");
        let out_src = dir.join("out-src.txt");
        let r_none =
            writeback_txt(&in_path, &tuples, BiStrategy::BilingualInline, &out_none).unwrap();
        let r_src =
            writeback_txt(&in_path, &with_src, BiStrategy::BilingualInline, &out_src).unwrap();
        assert_eq!(
            std::fs::read(&out_none).unwrap(),
            std::fs::read(&out_src).unwrap(),
            "None 与一致源逐字节等价"
        );
        assert_eq!((r_none.written, r_none.skipped.len()), (3, 0));
        assert_eq!((r_src.written, r_src.skipped.len()), (3, 0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 报告计数（部分命中）：无译文单元不记（正常未翻译）、未匹配 id 不记、
    /// 命中计入 written；OriginalPlusAppendix 的文尾附录写入同样计数。
    #[test]
    fn report_counts_partial_hits() {
        let dir = case_dir("report-counts");
        let in_path = dir.join("in.txt");
        std::fs::write(&in_path, "第一章 起\n\n正文一段句子。\n\n尾段完整句子。\n").unwrap();

        // 只给首尾两条（p0-b1 无译文 = 正常未翻译，不记 skipped），外加未匹配 id
        let pairs: Vec<crate::emit::TranslationPair> = vec![
            ("p0-b0".to_string(), "The beginning.".to_string()).into(),
            ("p0-b2".to_string(), "The ending.".to_string()).into(),
            ("p0-b99".to_string(), "bogus".to_string()).into(),
        ];
        let out_path = dir.join("out.txt");
        let report =
            writeback_txt(&in_path, &pairs, BiStrategy::OriginalPlusAppendix, &out_path).unwrap();
        assert_eq!(report.written, 2);
        assert!(report.skipped.is_empty(), "无译文与未匹配 id 都不是 skip");
        let out = std::fs::read_to_string(&out_path).unwrap();
        assert!(out.contains(APPENDIX_MARK));
        assert!(out.contains("The beginning.") && out.contains("The ending."));
        assert!(!out.contains("bogus"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
