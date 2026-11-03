// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! 字幕解析：srt / vtt / ass / ssa（零第三方依赖，语义对齐 Python `plugins/subtitle.py`）。
//!
//! 字幕不是纯文本：序号、`-->` 时间码、`{\pos(..)}` 覆盖块、VTT 内联标签都会污染下游
//! （TTS 会念出时间码），所以单独一个后端，而不是落到 txt。
//!
//! 约定（两载体逐字一致，见 FORMAT_TODO P3）：
//! * **一条字幕条目 = 一页**：`Page.name` = 规范化时间轴 `HH:MM:SS,mmm --> HH:MM:SS,mmm`
//!   （VTT 的 `.`、ASS 的厘秒都归一到这里），文本是该页唯一的 Paragraph 块，
//!   条目内换行保留为 `\n`。
//! * 文本清理：先剥 `<...>` 标签，再解 6 个实体（`&amp; &lt; &gt; &quot; &apos; &nbsp;`），
//!   两者顺序不能反，否则 `&lt;b&gt;` 会被当成标签吞掉。
//! * ASS/SSA 额外：删 `{...}` 覆盖块，`\N` / `\n` → 换行，`\h` → 空格。
//! * 清理后为空的条目跳过；一条不剩则 `corrupt`。
//! * 严格 UTF-8（可带 BOM）：猜编码会把中文念成乱码，宁可报错让调用方先转码。

use std::sync::Arc;

use crate::error::{DocError, Result};
use crate::model::{Block, BlockKind, Document, Page};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawPage, RawTextItem};
use crate::registry::Registry;

/// 支持的扩展名（`ssa` 与 `ass` 共用同一套事件表解析）。
pub const FORMATS: &[&str] = &["srt", "vtt", "ass", "ssa"];

/// 实体名 → 字符；未知实体（`&#123;` 等）原样保留。
fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => None,
    }
}

/// 剥离 `<...>` 标签；`<` 后没有 `>` 时按普通字符保留（不吞掉半个文本）。
pub fn strip_tags(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(off) = chars[i + 1..].iter().position(|c| *c == '>') {
                i += off + 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 只解 6 个常见实体；`&` 后 10 个字符内找不到 `;` 就按普通字符处理。
pub fn decode_entities(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&' {
            if let Some(off) = chars[i + 1..].iter().position(|c| *c == ';') {
                if off <= 9 {
                    let name: String = chars[i + 1..i + 1 + off].iter().collect();
                    if let Some(c) = entity(&name) {
                        out.push(c);
                        i += off + 2;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 标签 → 实体 → 逐行去空白；保留条目内换行。
pub fn clean_text(raw: &str) -> String {
    let body = decode_entities(&strip_tags(raw));
    body.split('\n')
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// 删除 ASS 覆盖块 `{...}`（`{\pos(..)}` / `{\i1}` 等）。
pub fn strip_ass_overrides(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            if let Some(off) = chars[i + 1..].iter().position(|c| *c == '}') {
                i += off + 2;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// ASS 文本：去覆盖块 → `\N` / `\n` 转换行 → `\h` 转空格。
pub fn ass_text(raw: &str) -> String {
    strip_ass_overrides(raw)
        .replace("\\N", "\n")
        .replace("\\n", "\n")
        .replace("\\h", " ")
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `[H:]MM:SS[.,]fff` → 毫秒；无法解析返回 `None`。
///
/// 小数位不足 3 位右补零（`.5` → 500ms，ASS 厘秒 `.50` → 500ms），多出 3 位截断。
pub fn parse_clock(spec: &str) -> Option<i64> {
    let s = spec.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let (head, tail) = parts.split_at(parts.len() - 1);
    if !head.iter().all(|p| all_digits(p)) {
        return None;
    }
    let tail = tail[0];
    let sep = if tail.contains(',') {
        ','
    } else if tail.contains('.') {
        '.'
    } else {
        return None;
    };
    let (sec, frac) = tail.split_once(sep)?;
    if !all_digits(sec) || !all_digits(frac) {
        return None;
    }
    let mut milli = frac.chars().take(3).collect::<String>();
    while milli.len() < 3 {
        milli.push('0');
    }
    let milli: i64 = milli.parse().ok()?;
    let (hours, minutes) = if head.len() == 2 {
        (head[0].parse::<i64>().ok()?, head[1].parse::<i64>().ok()?)
    } else {
        (0, head[0].parse::<i64>().ok()?)
    };
    let total = hours
        .saturating_mul(3600)
        .saturating_add(minutes.saturating_mul(60))
        .saturating_add(sec.parse::<i64>().ok()?);
    Some(total.saturating_mul(1000).saturating_add(milli))
}

/// 毫秒 → `HH:MM:SS,mmm`（小时不截断，不足两位补零）。
pub fn format_clock(ms: i64) -> String {
    let ms = if ms < 0 { 0 } else { ms };
    let milli = ms % 1000;
    let total = ms / 1000;
    let second = total % 60;
    let minutes_total = total / 60;
    let minute = minutes_total % 60;
    let hour = minutes_total / 60;
    format!("{hour:02}:{minute:02}:{second:02},{milli:03}")
}

/// `start --> end [settings]` → (start_ms, end_ms)；非法返回 `None`。
pub fn parse_timeline(line: &str) -> Option<(i64, i64)> {
    let pieces: Vec<&str> = line.split("-->").collect();
    if pieces.len() != 2 {
        return None;
    }
    let start = parse_clock(pieces[0])?;
    let end_field = pieces[1].split_whitespace().next()?;
    let end = parse_clock(end_field)?;
    Some((start, end))
}

fn timeline_name(tl: (i64, i64)) -> String {
    format!("{} --> {}", format_clock(tl.0), format_clock(tl.1))
}

/// 按空行分组；组内保留原始行（行尾空白去掉，组尾空行丢弃）。
pub fn split_blocks(text: &str) -> Vec<Vec<String>> {
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if !line.trim().is_empty() {
            buf.push(line.trim_end().to_string());
        } else if !buf.is_empty() {
            blocks.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        blocks.push(buf);
    }
    blocks
}

fn first_timeline_index(block: &[String]) -> Option<usize> {
    block.iter().position(|ln| ln.contains("-->"))
}

/// SRT：组内首个含 `-->` 的行是时间轴，其后全是文本；无时间轴的组跳过。
pub fn parse_srt(text: &str) -> Result<Vec<(String, String)>> {
    let mut cues = Vec::new();
    for block in split_blocks(text) {
        let Some(idx) = first_timeline_index(&block) else {
            continue;
        };
        let Some(tl) = parse_timeline(&block[idx]) else {
            let shown: String = block[idx].trim().chars().take(120).collect();
            return Err(DocError::corrupt(format!("SRT 时间轴格式非法: {shown}")));
        };
        cues.push((timeline_name(tl), block[idx + 1..].join("\n")));
    }
    Ok(cues)
}

/// VTT：首行必须 `WEBVTT`；`NOTE` / `STYLE` / `REGION` 块跳过；时间轴后可带设置。
pub fn parse_vtt(text: &str) -> Result<Vec<(String, String)>> {
    let first = text
        .split('\n')
        .find(|ln| !ln.trim().is_empty())
        .unwrap_or("");
    if !first.trim().to_uppercase().starts_with("WEBVTT") {
        return Err(DocError::corrupt("不是合法的 WebVTT（缺少 WEBVTT 头）"));
    }
    let mut cues = Vec::new();
    for block in split_blocks(text) {
        let head = block[0].trim();
        let keyword = head.split(' ').next().unwrap_or("").to_uppercase();
        if matches!(keyword.as_str(), "NOTE" | "STYLE" | "REGION") {
            continue;
        }
        if head.to_uppercase().starts_with("WEBVTT") {
            continue;
        }
        let Some(idx) = first_timeline_index(&block) else {
            continue;
        };
        let Some(tl) = parse_timeline(&block[idx]) else {
            let shown: String = block[idx].trim().chars().take(120).collect();
            return Err(DocError::corrupt(format!("VTT 时间轴格式非法: {shown}")));
        };
        cues.push((timeline_name(tl), block[idx + 1..].join("\n")));
    }
    Ok(cues)
}

fn lower_ascii(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// ASS/SSA：只吃 `[Events]` 段的 `Format:` + `Dialogue:`（`Comment:` 跳过）。
pub fn parse_ass(text: &str) -> Result<Vec<(String, String)>> {
    let mut fields: Option<Vec<String>> = None;
    let mut section = String::new();
    let mut cues = Vec::new();

    for line in text.split('\n') {
        let row = line.trim();
        if row.is_empty() {
            continue;
        }
        if row.starts_with('[') && row.ends_with(']') {
            section = lower_ascii(row[1..row.len() - 1].trim());
            continue;
        }
        if section != "events" {
            continue;
        }
        let low = lower_ascii(row);
        if low.starts_with("format:") {
            fields = Some(
                row["format:".len()..]
                    .split(',')
                    .map(|f| lower_ascii(f.trim()))
                    .collect(),
            );
            continue;
        }
        if low.starts_with("comment:") {
            continue;
        }
        if !low.starts_with("dialogue:") {
            continue;
        }
        let Some(names) = fields.as_ref() else {
            return Err(DocError::corrupt("ASS 的 Dialogue 出现在 Format 行之前"));
        };
        let payload = row["dialogue:".len()..].trim_start();
        // Text 是最后一个字段且可含逗号：限次拆分，剩余全部归最后一段
        let values: Vec<&str> = payload.splitn(names.len(), ',').collect();
        if values.len() != names.len() {
            return Err(DocError::corrupt(format!(
                "ASS Dialogue 字段数不足（期望 {}）",
                names.len()
            )));
        }
        let field = |name: &str| -> Option<&str> {
            names
                .iter()
                .position(|f| f == name)
                .map(|i| values[i].trim())
        };
        let (Some(start), Some(end)) = (
            field("start").and_then(parse_clock),
            field("end").and_then(parse_clock),
        ) else {
            let shown: String = payload.chars().take(120).collect();
            return Err(DocError::corrupt(format!("ASS 时间轴格式非法: {shown}")));
        };
        cues.push((
            timeline_name((start, end)),
            ass_text(field("text").unwrap_or("")),
        ));
    }

    if fields.is_none() {
        return Err(DocError::corrupt("ASS 未找到 [Events] 段的 Format 行"));
    }
    Ok(cues)
}

/// 严格 UTF-8（可带 BOM）读取并归一换行；非法字节 → `corrupt`。
pub fn read_subtitle(path: &str) -> Result<String> {
    let raw =
        std::fs::read(path).map_err(|e| DocError::corrupt(format!("无法读取字幕文件: {e}")))?;
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
    let text = std::str::from_utf8(raw)
        .map_err(|_| DocError::corrupt("字幕文件不是合法 UTF-8：请另存为 UTF-8 后重试"))?;
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// 解析成 `[(规范化时间轴, 已清理文本)]`，空白条目丢弃；一条不剩 → `corrupt`。
pub fn parse_cues(path: &str, kind: &str) -> Result<Vec<(String, String)>> {
    let text = read_subtitle(path)?;
    let raw = match kind {
        "vtt" => parse_vtt(&text)?,
        "ass" | "ssa" => parse_ass(&text)?,
        _ => parse_srt(&text)?,
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, body) in raw {
        let cleaned = clean_text(&body);
        if !cleaned.is_empty() {
            out.push((name, cleaned));
        }
    }
    if out.is_empty() {
        return Err(DocError::corrupt(format!("{kind} 未找到任何非空字幕条目")));
    }
    Ok(out)
}

/// 字幕解析后端（一个实现覆盖 srt/vtt/ass/ssa，靠 `kind` 分派）。
pub struct SubtitleParseCapability {
    pub id: &'static str,
    pub kind: &'static str,
}

impl ParseCapability for SubtitleParseCapability {
    fn id(&self) -> &str {
        self.id
    }
    fn formats(&self) -> &[&str] {
        match self.kind {
            "vtt" => &["vtt"],
            "ass" => &["ass", "ssa"],
            _ => &["srt"],
        }
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let cues = parse_cues(path, self.kind)?;
        let pages = cues
            .into_iter()
            .map(|(name, text)| RawPage {
                name: Some(name),
                items: vec![RawTextItem {
                    text,
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect();
        Ok(RawDoc {
            pages,
            ..Default::default()
        })
    }
}

/// 字幕插件：每条一页，页内一个 Paragraph 块（与 Python `SubtitlePlugin` 同构）。
pub struct SubtitlePlugin;

impl FormatPlugin for SubtitlePlugin {
    fn id(&self) -> &str {
        "subtitle"
    }
    fn formats(&self) -> &[&str] {
        FORMATS
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec![
            "subtitle.read.srt",
            "subtitle.read.vtt",
            "subtitle.read.ass",
        ]
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        vec![]
    }
    fn assemble(&self, raw: &RawDoc) -> Document {
        let pages = raw
            .pages
            .iter()
            .map(|rp| {
                let blocks: Vec<Block> = rp
                    .items
                    .iter()
                    .map(|item| Block {
                        kind: BlockKind::Paragraph,
                        content: item.text.clone(),
                        ..Default::default()
                    })
                    .collect();
                let text = rp
                    .items
                    .iter()
                    .map(|i| i.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                Page {
                    name: rp.name.clone(),
                    blocks,
                    text,
                    ..Default::default()
                }
            })
            .collect();
        Document {
            pages,
            ..Default::default()
        }
    }
}

/// 注册字幕后端与插件（无第三方依赖，始终编译）。
pub fn register_defaults(reg: &mut Registry) {
    for (id, kind) in [
        ("subtitle.read.srt", "srt"),
        ("subtitle.read.vtt", "vtt"),
        ("subtitle.read.ass", "ass"),
    ] {
        let _ = reg.register_parse(Arc::new(SubtitleParseCapability { id, kind }), None);
    }
    reg.register_plugin(Arc::new(SubtitlePlugin));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_parses_and_normalizes() {
        assert_eq!(parse_clock("00:00:01,000"), Some(1000));
        assert_eq!(parse_clock("0:00:03.50"), Some(3500));
        assert_eq!(parse_clock("01:02:03.004"), Some(3_723_004));
        assert_eq!(parse_clock(" 1:00 "), None); // 缺小数
        assert_eq!(parse_clock("abc"), None);
        assert_eq!(format_clock(3_723_004), "01:02:03,004");
        assert_eq!(format_clock(500), "00:00:00,500");
    }

    #[test]
    fn timeline_ignores_trailing_settings() {
        assert_eq!(
            parse_timeline("00:00:01,000 --> 00:00:02,500 X1:0 X2:1"),
            Some((1000, 2500))
        );
        assert_eq!(
            parse_timeline("00:01.000 --> 00:02.000 line:0 align:start"),
            Some((1000, 2000))
        );
        assert_eq!(parse_timeline("no arrow"), None);
    }

    #[test]
    fn srt_cues_keep_multiline_and_strip_tags() {
        let text = "1\n00:00:01,000 --> 00:00:04,500\n第一句\n<i>第二行</i>\n\n2\n00:00:05,000 --> 00:00:07,000\nA &amp; B\n";
        let cues = parse_srt(text).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].0, "00:00:01,000 --> 00:00:04,500");
        assert_eq!(clean_text(&cues[0].1), "第一句\n第二行");
        assert_eq!(clean_text(&cues[1].1), "A & B");
    }

    #[test]
    fn srt_missing_arrow_blocks_are_skipped() {
        let text = "1\n00:00:01,000 --> 00:00:02,000\n正文\n\n游离文本\n";
        assert_eq!(parse_srt(text).unwrap().len(), 1);
    }

    #[test]
    fn vtt_skips_note_and_requires_header() {
        let text = "WEBVTT - 标题\n\nNOTE 注释\n不该出现\n\nc1\n00:00:01.000 --> 00:00:02.500 line:0\n<v 旁白>你好</v>\n";
        let cues = parse_vtt(text).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(clean_text(&cues[0].1), "你好");

        let err = parse_vtt("1\n00:00:01,000 --> 00:00:02,000\nx\n").unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
    }

    #[test]
    fn ass_keeps_commas_in_text_and_drops_overrides() {
        let text = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:03.50,Default,,0,0,0,,{\\pos(1,2)}你好，世界\\N第二行\nComment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,忽略\n";
        let cues = parse_ass(text).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].0, "00:00:01,000 --> 00:00:03,500");
        assert_eq!(clean_text(&cues[0].1), "你好，世界\n第二行");
    }

    #[test]
    fn ass_without_events_is_corrupt() {
        let err = parse_ass("[Script Info]\nTitle: t\n").unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
    }

    #[test]
    fn registry_resolves_every_subtitle_format() {
        let mut reg = Registry::new();
        crate::plugins::register_defaults(&mut reg);
        let pol = crate::policy::BackendPolicy::default();
        let default = [
            "subtitle.read.srt",
            "subtitle.read.vtt",
            "subtitle.read.ass",
        ];
        for (fmt, want) in [
            ("srt", "subtitle.read.srt"),
            ("vtt", "subtitle.read.vtt"),
            ("ass", "subtitle.read.ass"),
            ("ssa", "subtitle.read.ass"),
        ] {
            let chain = reg.resolve_parse_chain(fmt, &pol, &default);
            assert_eq!(chain.len(), 1, "{fmt} 解析链为空");
            assert_eq!(chain[0].id(), want, "{fmt}");
        }
    }

    #[test]
    fn engine_parses_subtitle_end_to_end() {
        let dir = std::env::temp_dir();
        let cases: [(&str, &str, &str); 3] = [
            (
                "e2e.srt",
                "1\n00:00:01,000 --> 00:00:02,000\n第一句\n",
                "第一句",
            ),
            (
                "e2e.vtt",
                "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\n你好\n",
                "你好",
            ),
            (
                "e2e.ass",
                "[Events]\nFormat: Layer, Start, End, Text\nDialogue: 0,0:00:01.00,0:00:02.50,台词\n",
                "台词",
            ),
        ];
        for (name, body, want) in cases {
            let p = dir.join(format!("docparser_{name}"));
            std::fs::write(&p, body).unwrap();
            let mut reg = Registry::new();
            crate::plugins::register_defaults(&mut reg);
            let doc = crate::engine::DocumentEngine::new(reg)
                .parse(p.to_str().unwrap())
                .unwrap_or_else(|e| panic!("{name} 解析失败: {e}"));
            assert_eq!(doc.pages.len(), 1, "{name}");
            assert_eq!(doc.pages[0].text, want, "{name}");
            std::fs::remove_file(&p).ok();
        }
    }

    #[test]
    fn plugin_makes_one_page_per_cue() {
        let raw = RawDoc {
            pages: vec![RawPage {
                name: Some("00:00:01,000 --> 00:00:02,000".into()),
                items: vec![RawTextItem {
                    text: "台词".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let doc = SubtitlePlugin.assemble(&raw);
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(
            doc.pages[0].name.as_deref(),
            Some("00:00:01,000 --> 00:00:02,000")
        );
        assert_eq!(doc.pages[0].text, "台词");
        assert_eq!(doc.pages[0].blocks.len(), 1);
    }
}
