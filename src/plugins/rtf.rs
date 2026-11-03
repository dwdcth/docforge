// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! RTF 插件：`.rtf`（纯 Rust 状态机，语义对齐 Python `docparser.plugins.rtf`）。
//!
//! 零第三方依赖：RTF 正文是「控制词 + 组 + 转义字节」的字节流，抽纯文本不需要
//! `striprtf` 级别的完整实现，也就不为它引依赖。
//!
//! 口径（两载体一致，`bench/office_parity.py` 逐块比对）：
//!
//! * 输出**单页**，每个 `\par` / `\row` / `\sect` 结束一个段落（一个 `Paragraph` 块），
//!   `\line` 是段内换行，`\tab` / `\cell` → 制表符；
//! * 页文本 = 段落以空行连接；空段落丢弃；整份文件无正文 → `corrupt`；
//! * 跳过目标组：`{\*...}`、`\fonttbl` `\colortbl` `\stylesheet` `\info` `\pict`
//!   `\object` `\fldinst` 页眉页脚/脚注/批注等（见 [`SKIP_DESTS`]）；
//!   `{\fldrslt 显示文字}` 保留文字；
//! * `\uN` 走 Unicode（`\ucN` 回退跳过、代理对合并）；`\'hh` 按 `\ansicpg` 解：
//!   1252（缺省，查表单字节映射）/ 65001（收集原始字节，段尾统一校验 UTF-8），
//!   其它代码页（如 936）显式 `corrupt` 而不是吐乱码；同一段里「原生 UTF-8 汉字 +
//!   `\'hh` 拉丁字符」能各自正确解出；
//! * 转义：`\\` `\{` `\}` 取字面；`\~` → 不换行空格；`\_` → `-`；`\-` 丢弃；
//!   `\` + 换行 ≡ `\par`；`\binN` 跳过 N 字节原始数据；
//! * 源文件里的裸 CR/LF/TAB 是排版空白，忽略；未知控制词忽略。
//!
//! 已知不做：样式/字体/颜色、页眉页脚内容、图片、表格结构（摊平成制表符+行）、
//! 多节（`\sect` 只断段不换页）、`\uc` 的组作用域（按全局处理）。

use std::sync::Arc;

use crate::error::{DocError, Result};
use crate::model::{Block, BlockKind, Document, Page};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawPage, RawTextItem};
use crate::registry::Registry;

/// RTF 文件头（`{\rtf`）。
pub const MAGIC: &[u8] = b"{\\rtf";

/// 这些目标组只装元数据/二进制，正文不该进 IR（`\*` 已单独处理）。
pub const SKIP_DESTS: &[&str] = &[
    "fonttbl",
    "colortbl",
    "stylesheet",
    "info",
    "pict",
    "object",
    "header",
    "footer",
    "headerl",
    "headerr",
    "headerf",
    "footerl",
    "footerr",
    "footerf",
    "footnote",
    "annotation",
    "atnid",
    "atnauthor",
    "fldinst",
    "themedata",
    "datastore",
    "latentstyles",
    "listtable",
    "listoverridetable",
    "rsidtbl",
    "generator",
    "xmlnstbl",
    "filetbl",
    "revtbl",
    "pn",
    "sp",
    "sn",
    "sv",
    "shpinst",
    "background",
    "do",
    "buptim",
    "userprops",
];

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn push_char(buf: &mut Vec<u8>, ch: char) {
    let mut tmp = [0u8; 4];
    buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
}

/// CP1252 单字节映射（未定义字节 0x81/0x8D/0x8F/0x90/0x9D → U+FFFD）。
///
/// 必须与 Python `bytes.decode("cp1252", errors="replace")` 完全一致。
fn cp1252_char(b: u8) -> char {
    match b {
        0x80 => '\u{20AC}',
        0x81 => '\u{FFFD}',
        0x82 => '\u{201A}',
        0x83 => '\u{0192}',
        0x84 => '\u{201E}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02C6}',
        0x89 => '\u{2030}',
        0x8A => '\u{0160}',
        0x8B => '\u{2039}',
        0x8C => '\u{0152}',
        0x8D => '\u{FFFD}',
        0x8E => '\u{017D}',
        0x8F => '\u{FFFD}',
        0x90 => '\u{FFFD}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02DC}',
        0x99 => '\u{2122}',
        0x9A => '\u{0161}',
        0x9B => '\u{203A}',
        0x9C => '\u{0153}',
        0x9D => '\u{FFFD}',
        0x9E => '\u{017E}',
        0x9F => '\u{0178}',
        _ => b as char, // 0x00-0x7F 与 0xA0-0xFF 与 Latin-1 相同
    }
}

fn flush(buf: &mut Vec<u8>, pending_high: &mut Option<u32>, out: &mut Vec<String>) -> Result<()> {
    if pending_high.is_some() {
        push_char(buf, '\u{FFFD}');
        *pending_high = None;
    }
    let text = std::str::from_utf8(buf).map_err(|_| {
        DocError::corrupt("RTF 正文含非法 UTF-8 字节（ansicpg65001 下 \\'hh 序列不完整）")
    })?;
    let trimmed = text.trim();
    let paragraph = trimmed.to_string();
    buf.clear();
    if !paragraph.is_empty() {
        out.push(paragraph);
    }
    Ok(())
}

/// RTF 字节 → 段落列表（已 trim、空段落已丢弃）。
pub fn extract_paragraphs(raw: &[u8]) -> Result<Vec<String>> {
    let data = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    let mut start = 0;
    while start < data.len() && matches!(data[start], b' ' | b'\t' | b'\r' | b'\n') {
        start += 1;
    }
    let data = &data[start..];
    if !data.starts_with(MAGIC) {
        return Err(DocError::corrupt("不是合法的 RTF（缺少 {\\rtf 头）"));
    }

    let n = data.len();
    let mut paragraphs: Vec<String> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut codepage: Option<i32> = None;
    let mut uc: i32 = 1;
    let mut skip_left: i32 = 0;
    let mut pending_high: Option<u32> = None;
    let mut depth: usize = 0;
    let mut skip_from: Option<usize> = None;
    let mut dest_pending: Vec<bool> = Vec::new();

    let mut i = 0usize;
    while i < n {
        let c = data[i];

        if c == b'{' {
            depth += 1;
            dest_pending.push(true);
            i += 1;
            continue;
        }

        if c == b'}' {
            if let Some(s) = skip_from {
                if s >= depth {
                    skip_from = None;
                }
            }
            if depth > 0 {
                depth -= 1;
                dest_pending.pop();
            }
            i += 1;
            continue;
        }

        if c == b'\\' {
            if i + 1 >= n {
                i += 1;
                continue;
            }
            let nxt = data[i + 1];

            // 控制词：[a-zA-Z]+ 可选 -?digits 可选单个空格分隔符
            if nxt.is_ascii_alphabetic() {
                let mut j = i + 1;
                while j < n && data[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let word = String::from_utf8_lossy(&data[i + 1..j]).to_ascii_lowercase();
                let mut k = j;
                let mut neg = false;
                if k < n && data[k] == b'-' {
                    neg = true;
                    k += 1;
                }
                let digits_at = k;
                while k < n && data[k].is_ascii_digit() {
                    k += 1;
                }
                let mut param: Option<i32> = None;
                if k > digits_at {
                    let v: i32 = String::from_utf8_lossy(&data[digits_at..k])
                        .parse()
                        .unwrap_or(0);
                    param = Some(if neg { -v } else { v });
                }
                if k < n && data[k] == b' ' {
                    k += 1;
                }
                i = k;

                let first_in_group = dest_pending.last().copied().unwrap_or(false);
                if first_in_group {
                    if let Some(last) = dest_pending.last_mut() {
                        *last = false;
                    }
                    if SKIP_DESTS.contains(&word.as_str()) {
                        if skip_from.is_none() {
                            skip_from = Some(depth);
                        }
                        continue;
                    }
                }
                if skip_from.is_some() {
                    if word == "bin" {
                        if let Some(p) = param {
                            if p > 0 {
                                i = (i + p as usize).min(n);
                            }
                        }
                    }
                    continue;
                }

                match word.as_str() {
                    "par" | "row" | "sect" => {
                        flush(&mut buf, &mut pending_high, &mut paragraphs)?;
                    }
                    "line" => buf.push(b'\n'),
                    "tab" | "cell" => buf.push(b'\t'),
                    "uc" => uc = param.unwrap_or(0).max(0),
                    "ansicpg" => {
                        if let Some(p) = param {
                            codepage = Some(p);
                        }
                    }
                    "bin" => {
                        if let Some(p) = param {
                            if p > 0 {
                                i = (i + p as usize).min(n);
                            }
                        }
                    }
                    "u" => {
                        let mut raw_cp = param.unwrap_or(0);
                        if raw_cp < 0 {
                            raw_cp += 0x10000;
                        }
                        let cp = raw_cp as u32;
                        if (0xD800..=0xDBFF).contains(&cp) {
                            if pending_high.is_some() {
                                push_char(&mut buf, '\u{FFFD}');
                            }
                            pending_high = Some(cp);
                        } else if (0xDC00..=0xDFFF).contains(&cp) && pending_high.is_some() {
                            let hi = pending_high.take().unwrap_or(0xD800);
                            let full = 0x10000 + ((hi - 0xD800) << 10) + (cp - 0xDC00);
                            push_char(&mut buf, char::from_u32(full).unwrap_or('\u{FFFD}'));
                        } else {
                            if pending_high.is_some() {
                                push_char(&mut buf, '\u{FFFD}');
                                pending_high = None;
                            }
                            let ch = if (0xDC00..=0xDFFF).contains(&cp) {
                                '\u{FFFD}'
                            } else {
                                char::from_u32(cp).unwrap_or('\u{FFFD}')
                            };
                            push_char(&mut buf, ch);
                        }
                        skip_left = uc;
                    }
                    _ => {}
                }
                continue;
            }

            // 控制符号
            if nxt == b'\n' || nxt == b'\r' {
                let mut j = i + 1;
                while j < n && (data[j] == b'\n' || data[j] == b'\r') {
                    j += 1;
                }
                i = j;
                if skip_from.is_none() {
                    flush(&mut buf, &mut pending_high, &mut paragraphs)?;
                }
                continue;
            }

            if nxt == b'\'' {
                if i + 3 >= n {
                    return Err(DocError::corrupt("RTF 的 \\'hh 转义不完整"));
                }
                let hi = hexval(data[i + 2])
                    .ok_or_else(|| DocError::corrupt("RTF 的 \\'hh 转义含非十六进制字符"))?;
                let lo = hexval(data[i + 3])
                    .ok_or_else(|| DocError::corrupt("RTF 的 \\'hh 转义含非十六进制字符"))?;
                i += 4;
                if skip_from.is_some() {
                    continue;
                }
                if skip_left > 0 {
                    skip_left -= 1;
                    continue;
                }
                let byte = hi * 16 + lo;
                match codepage {
                    Some(65001) => buf.push(byte), // UTF-8 字节流，段尾统一校验
                    Some(cp) if cp != 1252 => {
                        return Err(DocError::corrupt(format!(
                            "RTF 声明了不支持的代码页 {cp}（仅支持 1252 / 65001）；请另存为 Unicode RTF"
                        )));
                    }
                    _ => push_char(&mut buf, cp1252_char(byte)),
                }
                continue;
            }

            if nxt == b'*' {
                let first_in_group = dest_pending.last().copied().unwrap_or(false);
                if first_in_group {
                    if let Some(last) = dest_pending.last_mut() {
                        *last = false;
                    }
                    if skip_from.is_none() {
                        skip_from = Some(depth);
                    }
                }
                i += 2;
                continue;
            }

            if skip_from.is_some() {
                i += 2;
                continue;
            }

            match nxt {
                b'\\' => buf.push(b'\\'),
                b'{' => buf.push(b'{'),
                b'}' => buf.push(b'}'),
                b'~' => push_char(&mut buf, '\u{00A0}'),
                b'_' => buf.push(b'-'),
                _ => {}
            }
            i += 2;
            continue;
        }

        // 普通字节
        if c < 0x20 {
            i += 1;
            continue;
        }
        if skip_from.is_some() {
            i += 1;
            continue;
        }
        if skip_left > 0 {
            skip_left -= 1;
            i += 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }

    flush(&mut buf, &mut pending_high, &mut paragraphs)?;
    if paragraphs.is_empty() {
        return Err(DocError::corrupt("RTF 未找到任何正文文本"));
    }
    Ok(paragraphs)
}

/// RTF 解析后端：段落 → `RawTextItem`。
pub struct RtfParseCapability;

impl ParseCapability for RtfParseCapability {
    fn id(&self) -> &str {
        "rtf.read.rtf"
    }
    fn formats(&self) -> &[&str] {
        &["rtf"]
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let raw = std::fs::read(path)
            .map_err(|e| DocError::corrupt(format!("无法读取 RTF 文件: {e}")))?;
        let items = extract_paragraphs(&raw)?
            .into_iter()
            .map(|text| RawTextItem {
                text,
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

/// RTF 插件：RawDoc（段落）→ IR（每段一个 `Paragraph` 块，单页）。
pub struct RtfPlugin;

impl FormatPlugin for RtfPlugin {
    fn id(&self) -> &str {
        "rtf"
    }
    fn formats(&self) -> &[&str] {
        &["rtf"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["rtf.read.rtf"]
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
                name: None,
                blocks,
                text: items.join("\n\n"),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
}

/// 注册 RTF 解析后端与插件。
pub fn register_defaults(reg: &mut Registry) {
    let _ = reg.register_parse(Arc::new(RtfParseCapability), None);
    reg.register_plugin(Arc::new(RtfPlugin));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paragraphs_and_skips_destinations() {
        let src = br"{\rtf1\ansi\ansicpg1252{\fonttbl{\f0 Arial;}}{\*\generator Foo;}\pard
first paragraph\par
{\b bold} text with \{braces\} and \\slash\par
}";
        let paras = extract_paragraphs(src).unwrap();
        assert_eq!(
            paras,
            vec!["first paragraph", "bold text with {braces} and \\slash"]
        );
    }

    #[test]
    fn unicode_and_hex_escapes_decode() {
        let src = br"{\rtf1\u20320?\u22909?\par caf\'e9 \'93quoted\'94\par}";
        let paras = extract_paragraphs(src).unwrap();
        assert_eq!(paras, vec!["你好", "caf\u{e9} \u{201c}quoted\u{201d}"]);
    }

    #[test]
    fn surrogate_pairs_merge_and_uc_skips_fallback() {
        // U+1F600 = D83D DE00，\uc1 时每个 \u 各跳 1 个回退字符
        let src = br"{\rtf1\uc1\u-10179?\u-8704?\par}";
        let paras = extract_paragraphs(src).unwrap();
        assert_eq!(paras, vec!["\u{1F600}"]);
    }

    #[test]
    fn controls_map_to_tabs_and_soft_breaks() {
        let src = br"{\rtf1 a\tab b\line c\par\cell d\row}";
        let paras = extract_paragraphs(src).unwrap();
        assert_eq!(paras, vec!["a\tb\nc", "d"]);
    }

    #[test]
    fn missing_header_is_corrupt() {
        let err = extract_paragraphs(b"hello world").unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
    }

    #[test]
    fn empty_body_is_corrupt() {
        let err = extract_paragraphs(br"{\rtf1\ansi{\fonttbl{\f0 Arial;}}}").unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
    }

    #[test]
    fn unsupported_codepage_with_raw_bytes_is_corrupt() {
        // 声明 936 且正文含非法 UTF-8 字节：宁可报错也不吐乱码
        let err = extract_paragraphs(br"{\rtf1\ansicpg936 \'d6\'d0\par}").unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(err.to_string().contains("936"));
    }

    #[test]
    fn registry_resolves_rtf_format() {
        let mut reg = Registry::new();
        crate::plugins::register_defaults(&mut reg);
        let pol = crate::policy::BackendPolicy::default();
        let chain = reg.resolve_parse_chain("rtf", &pol, &["rtf.read.rtf"]);
        assert_eq!(chain.len(), 1, "rtf 解析链为空");
        assert_eq!(chain[0].id(), "rtf.read.rtf");
    }

    #[test]
    fn engine_parses_rtf_end_to_end() {
        let p = std::env::temp_dir().join("docparser_e2e.rtf");
        std::fs::write(
            &p,
            br"{\rtf1\ansi{\fonttbl{\f0 Arial;}}Hello\par World\par}",
        )
        .unwrap();
        let mut reg = Registry::new();
        crate::plugins::register_defaults(&mut reg);
        let doc = crate::engine::DocumentEngine::new(reg)
            .parse(p.to_str().unwrap())
            .unwrap_or_else(|e| panic!("rtf 解析失败: {e}"));
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].text, "Hello\n\nWorld");
        assert_eq!(doc.pages[0].blocks.len(), 2);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn plugin_makes_one_block_per_paragraph() {
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
        let doc = RtfPlugin.assemble(&raw);
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].blocks.len(), 2);
        assert_eq!(doc.pages[0].text, "p1\n\np2");
    }
}
