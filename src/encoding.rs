//! 编码检测与解码（横切能力）。
//!
//! 原实现住在 [`crate::plugins::text`]（格式插件文件），但"字节 → String"
//! 的解码链语义上是横切能力：TXT/MD/CSV/HTML 共用
//! [`read_text`](crate::plugins::text::read_text) 一条链；EPUB 条目解码
//! （[`crate::backends::epub`]）在 XML 声明嗅探未命中时回落到本模块；
//! 外部消费者（rumi 等）可用 [`detect_and_decode`] 拿"解码结果 + 编码名"。
//! M2 后抽出为独立模块，[`decode`] 保持原签名与行为（text.rs 以 `pub use`
//! 转发，旧调用点零改动）。
//!
//! # 解码链（顺序即优先级）
//!
//! 1. UTF-8 BOM（`EF BB BF`）→ 剥 BOM 按 UTF-8；
//! 2. UTF-16 BOM（`FF FE` / `FE FF`）→ UTF-16LE / UTF-16BE；
//! 3. UTF-8 strict（绝大多数现代文件在此命中）；
//! 4. feature `encoding`（M2 中文编码链）：
//!    a. chardetng 概率探测（置信闸门见下）——覆盖 Big5/Shift-JIS/EUC-KR/
//!       Windows-125x 等固定链之外的编码；
//!    b. GB18030 固定回退（GBK 家族，要求全程无解码错误）；
//!    c. Big5 固定回退（同上）；
//! 5. 全链未命中 → lossy UTF-8（替换字符）。
//!
//! # 置信闸门（chardetng 段，两道）
//!
//! ① `guess_assess` 的 bool：探测编码须胜过至少一个候选，false = 探测
//!    "多半是错的"；
//! ② 最小样本量 8 字节：频率模型要统计证据，实测 <8 字节的 GBK 高频字节
//!    会被 windows-1252/EUC-KR 劫持且解码无错，≥8 字节 CJK 判别稳定。
//!    探测命中 UTF-8 直接弃（strict 已失败 = 非法 UTF-8 混合件，探测只是
//!    认出字节骨架）；闸门放行但探测编码解码有错也弃。
//!
//! # [`detect_and_decode`] 返回的编码名（`&'static str`）
//!
//! - `"utf-8"`：UTF-8（含 BOM 剥离）；
//! - `"utf-16le"` / `"utf-16be"`：UTF-16 BOM 命中；
//! - chardetng 命中：经内部归一（`canonical_name`）——GB18030 家族（chardetng
//!   命中的是 encoding_rs 的 `GBK` 解码器，固定链用的是 `GB18030` 解码器，
//!   两者同属 gb18030 编码）统一报 `"gb18030"`、Big5 报 `"big5"`，其余
//!   原样取 encoding_rs 名（`Encoding::name()`，如 `"Shift_JIS"` /
//!   `"windows-1252"` / `"EUC-KR"`）；
//! - `"gb18030"` / `"big5"`：固定回退链命中；
//! - `"lossy-utf8"`：全链未命中，替换字符兜底（未开 `encoding` feature 时
//!   非 UTF-8 输入一律落到这里）。

/// 字节 → 字符串（BOM 感知，失败退化为 lossy）。
///
/// 签名与语义同 M2 的 `plugins::text::decode`（逻辑已迁入本模块）。
pub fn decode(raw: Vec<u8>) -> String {
    let has_bom = raw.starts_with(&[0xEF, 0xBB, 0xBF])
        || raw.starts_with(&[0xFF, 0xFE])
        || raw.starts_with(&[0xFE, 0xFF]);
    if !has_bom {
        // 无 BOM 且有效 UTF-8：零拷贝快速路径（最常见情形）。
        // from_utf8 失败时字节从 Err 里取回，继续走统一探测链。
        match String::from_utf8(raw) {
            Ok(s) => return s,
            Err(e) => return detect_and_decode(e.as_bytes()).0,
        }
    }
    detect_and_decode(&raw).0
}

/// encoding_rs `Encoding` → 归一编码名：GB18030 家族统一报 "gb18030"、
/// Big5 报 "big5"、UTF-16 报小写名，其余原样取 `Encoding::name()`。
/// chardetng 段与固定链段共用，保证同一编码无论从哪一段命中，
/// [`detect_and_decode`] 报出的名字一致。
#[cfg(feature = "encoding")]
fn canonical_name(enc: &'static encoding_rs::Encoding) -> &'static str {
    // encoding_rs 的 GB18030 家族有两个实现态：`GB18030`（GB18030-2022
    // 解码器，名 "gb18030"，固定链用它）与 `GBK`（1998 经典 GBK 解码器，
    // 名 "GBK"，chardetng 命中的就是它）——两者同属 Encoding Standard 的
    // gb18030 编码，对外统一归一为 "gb18030"
    if enc == encoding_rs::GB18030 || enc == encoding_rs::GBK {
        "gb18030"
    } else if enc == encoding_rs::BIG5 {
        "big5"
    } else if enc == encoding_rs::UTF_16LE {
        "utf-16le"
    } else if enc == encoding_rs::UTF_16BE {
        "utf-16be"
    } else {
        enc.name()
    }
}

/// 解码 + 编码名探测：返回（解码结果, 命中的编码名），编码名清单见模块
/// 注释。外部消费者（rumi 等）做"解码顺带知道命中编码"的探测入口。
pub fn detect_and_decode(bytes: &[u8]) -> (String, &'static str) {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return (String::from_utf8_lossy(rest).into_owned(), "utf-8");
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return (decode_utf16(rest, true), "utf-16le");
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return (decode_utf16(rest, false), "utf-16be");
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_owned(), "utf-8"),
        Err(_) => {
            // 中文/多编码回退链（GBK 家族优先，Big5 次之；各自要求全程无解码错误）
            #[cfg(feature = "encoding")]
            {
                // —— chardetng 概率探测段（md/txt/csv/html/epub 共用一条
                //    链，先于固定链）：置信闸门两道，见模块注释。——
                let mut det = chardetng::EncodingDetector::new();
                det.feed(bytes, true);
                let (enc, confident) = det.guess_assess(None, true);
                if confident && bytes.len() >= 8 && enc != encoding_rs::UTF_8 {
                    let (s, _, had_errors) = enc.decode(bytes);
                    if !had_errors {
                        return (s.into_owned(), canonical_name(enc));
                    }
                }
                // —— 固定链段（探测未采纳时兜底，M2 行为保持）：——
                let (s_gb, _, err) = encoding_rs::GB18030.decode(bytes);
                if !err {
                    return (s_gb.into_owned(), canonical_name(encoding_rs::GB18030));
                }
                let (s_b5, _, err) = encoding_rs::BIG5.decode(bytes);
                if !err {
                    return (s_b5.into_owned(), canonical_name(encoding_rs::BIG5));
                }
            }
            // 未开 encoding feature：GBK 等单/多字节中文编码退化为替换字符，
            // 中文内容请以 UTF-8 保存（docparser 原注释语义保留）
            (String::from_utf8_lossy(bytes).into_owned(), "lossy-utf8")
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    #[cfg(feature = "encoding")]
    fn gb18030_decoded_when_feature() {
        // "你好，世界" 的 GBK 编码（非 UTF-8，无 BOM）
        let gbk: Vec<u8> = vec![0xC4, 0xE3, 0xBA, 0xC3, 0xA3, 0xAC, 0xCA, 0xC0, 0xBD, 0xE7];
        assert_eq!(decode(gbk), "你好，世界");
        // UTF-8 优先不受影响
        assert_eq!(decode("常规".as_bytes().to_vec()), "常规");
    }

    #[test]
    #[cfg(feature = "encoding")]
    fn big5_text_detected_via_chardet() {
        // Big5 长文本（≥200 字符）：Big5 字节同样能被 GB18030 无错解码成
        // 乱码，所以固定链拿不到正确结果——逐字还原只能来自 chardetng
        // 探测命中 Big5（置信闸门放行）
        let line = "這是一份測試文件，用來驗證編碼自動識別對繁體中文內容的效果。";
        let text = line.repeat(8);
        assert!(text.chars().count() >= 200);
        let (bytes, _, had_errors) = encoding_rs::BIG5.encode(&text);
        assert!(!had_errors);
        assert_eq!(decode(bytes.into_owned()), text);
    }

    #[test]
    #[cfg(feature = "encoding")]
    fn short_ambiguous_falls_back_to_fixed_chain() {
        // 短且歧义的 GBK 字节：低于最小探测样本量（8 字节），置信闸门不放行
        // → 固定链 GB18030 行为与 M2 保持一致
        // "你好瞒" 的 GBK（6 字节）
        assert_eq!(
            decode(vec![0xC4, 0xE3, 0xBA, 0xC3, 0xC2, 0xF7]),
            "你好瞒"
        );
        // "你" 的 GBK（2 字节）
        assert_eq!(decode(vec![0xC4, 0xE3]), "你");
    }

    #[test]
    fn detect_and_decode_names_utf_family() {
        assert_eq!(detect_and_decode("常规".as_bytes()), ("常规".to_string(), "utf-8"));

        // UTF-8 BOM：剥 BOM，命中 "utf-8"
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice("你好".as_bytes());
        assert_eq!(detect_and_decode(&bom), ("你好".to_string(), "utf-8"));

        // UTF-16LE / UTF-16BE BOM
        let mut le = vec![0xFF, 0xFE];
        for u in "ab".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(detect_and_decode(&le), ("ab".to_string(), "utf-16le"));

        let mut be = vec![0xFE, 0xFF];
        for u in "ab".encode_utf16() {
            be.extend_from_slice(&u.to_be_bytes());
        }
        assert_eq!(detect_and_decode(&be), ("ab".to_string(), "utf-16be"));

        // 全链未命中（未开 encoding 时非 UTF-8 的唯一去向）
        assert_eq!(detect_and_decode(&[0x61, 0xFF, 0x62]).1, "lossy-utf8");
    }

    #[test]
    #[cfg(feature = "encoding")]
    fn detect_and_decode_names_cjk() {
        // GBK（GB18030 家族）："你好，世界" 10 字节，探测/固定链命中名一致
        let gbk: Vec<u8> = vec![0xC4, 0xE3, 0xBA, 0xC3, 0xA3, 0xAC, 0xCA, 0xC0, 0xBD, 0xE7];
        let (s, name) = detect_and_decode(&gbk);
        assert_eq!(s, "你好，世界");
        assert_eq!(name, "gb18030");

        // Big5 长文本：chardetng 置信闸门放行后命中 "big5"
        let line = "這是一份測試文件，用來驗證編碼自動識別對繁體中文內容的效果。";
        let text = line.repeat(8);
        let (bytes, _, had_errors) = encoding_rs::BIG5.encode(&text);
        assert!(!had_errors);
        let (s, name) = detect_and_decode(&bytes);
        assert_eq!(s, text);
        assert_eq!(name, "big5");
    }
}
