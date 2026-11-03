//! azw3/KF8 记录解析（`super::MobiRawBackend` 的 KF8 支路，feature = "mobi"）。
//!
//! 与 MOBI7 通路（mobi crate）的关系：本模块完全自读 PalmDB 容器与 KF8
//! 记录结构，不经 mobi crate——它对 KF8 文件打开不报错但正文是乱码
//! （KF8 记录是 skeleton/fragment 结构且每条带 trailing bytes，crate
//! 无对应处理，见 src/backends/mobi.rs 模块头 M5 裁定）。
//!
//! 支持面与取舍：
//! * **混合件（MOBI7+KF8 同包）优先取 KF8 部分**：EXTH 记录 121
//!   （KF8 Boundary，u32 BE = KF8 头记录号）命中即剥离前段，只解析
//!   KF8 部分——翻译场景新格式优先（KF8 是 kindlegen 现行输出）。
//!   121 缺失/非法视为无 KF8 部分回落 MOBI7 通路；无 boundary 但
//!   MOBI 头 `format_version == 8`（纯 KF8，Previewer "KF8 only"
//!   输出形态）按整文件解析。
//! * **正文还原**：rawML = 记录 1..text_record_count 各自剥 trailing
//!   entry 后按压缩类型解压拼接 → FDST 切 flow（flow 0 为正文区）→
//!   按 SKEL/FRAG 两个 INDX 索引表组装：每个 skeleton 是一个 XHTML
//!   part 骨架，fragment 按各自 insert 位置插回（KindleUnpack 家族
//!   同款装配语义）。aid 属性、`kindle:pos:` 内链等 filepos 信息保留
//!   在 HTML 里（块化不受影响；翻译场景不消费内链定位）。
//! * **trailing entry 剥离**：`extra_flags`（记录 0 内偏移 0xF2，u16）
//!   bit0 = multibyte overlap（大小 = 末字节低 2 位 + 1），其余置位
//!   位数为通用 trailing entry 个数（大小 = 末 ≤4 字节的反向 7 位
//!   变长编码），先通用后 multibyte。MOBI7 通路不做此剥离（mobi
//!   crate 缺陷族，见 M5 记录），KF8 必须做否则记录边界混入垃圾字节。
//! * **压缩**：无压缩(1)/PalmDoc(2)；HUFF/CDIC(17480) 为 KF7 独有
//!   → corrupt 交上层回退链。
//! * **DRM**：encrypted 是终止码不回退（docparse-plan §6）——KF8 头
//!   PalmDoc encryption 字段或 drm_offset 任一命中即拒绝；已识别为
//!   KF8 后不再回落 mobi crate（防止混合件静默降级到 MOBI7 旧正文）。
//! * **编码**：KF8 规范文本编码恒为 UTF-8（65001）；1252 旧件按
//!   lossy 解码——与 MOBI7 通路"宁要占位符不要失败"同口径。
//!
//! 语义来源（合规声明，见 THIRD_PARTY_NOTICES.md）：
//! * **吸收代码**：PalmDoc LZ77 解压例程吸收自 ebook-rs 0.18.0（MIT，
//!   Copyright (c) 2026 SV-Stark）`vendor/ebook-rs-0.18.0/src/mobi.rs`
//!   `decompress_palmdoc`（自带 256 MiB 输出上界的抗炸实现），按本
//!   crate 形态收录。
//! * **仅格式语义参考（不吸收代码）**：KindleUnpack 家族（PyPI mobi
//!   0.4.1 及上游 kevinhendricks/KindleUnpack，GPL-3.0）——与 djvu
//!   0.1.0（M4）、rumi-ocr 同款裁定：PalmDB 偏移/EXTH/INDX/TAGX/
//!   trailing 编码等是格式事实，本模块为独立实现（结构、命名、注释
//!   均为本 crate 原创）。字段语义同时与 MobileRead MOBI wiki 及
//!   mobi-0.8.0 存档 headers 读取序交叉核对。
//!
//! 元数据回退口径与 MOBI7 通路一致（书名 EXTH 503 → 全名 → PalmDB
//! 名；作者 EXTH 100 全量；语言 EXTH 524 → locale 字节表，取不到不落
//! 键），meta 键同名（`mobi.title`/`mobi.authors`/`mobi.language`）
//! 以复用 [`super::MobiPlugin`] 的 assemble 透传。

use std::collections::HashMap;

use mobi::headers::Language;

use crate::error::{DocError, Result};
use crate::plugin::{RawDoc, RawPage, RawTextItem};

// ── 字节读取原语（全部越界安全，hostile-input 纪律同 crate 内其它后端）──

fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(off)?, *b.get(off + 1)?]))
}

fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(off)?,
        *b.get(off + 1)?,
        *b.get(off + 2)?,
        *b.get(off + 3)?,
    ]))
}

fn corrupt<T>(msg: impl Into<String>) -> Result<T> {
    Err(DocError::corrupt(msg.into()))
}

/// 反向 7 位变长值（高位置 1 终止）：INDX tag 值与 tag 字节长度的编码。
fn read_var_width(data: &[u8], off: usize) -> Option<(usize, u64)> {
    let mut value = 0u64;
    let mut consumed = 0usize;
    loop {
        let b = *data.get(off + consumed)?;
        consumed += 1;
        let finished = b & 0x80 != 0;
        value = (value << 7) | u64::from(b & 0x7F);
        if finished {
            return Some((consumed, value));
        }
    }
}

/// 通用 trailing entry 大小：末 ≤4 字节里、自最后一个高位置位字节起
/// 的 7 位组反向累积值。
fn trailing_entry_size(data: &[u8]) -> usize {
    let start = data.len().saturating_sub(4);
    let mut num = 0usize;
    for &v in &data[start..] {
        if v & 0x80 != 0 {
            num = 0;
        }
        num = (num << 7) | (v & 0x7F) as usize;
    }
    num
}

/// 按记录 0 的 `extra_flags`（偏移 0xF2 处 u16）剥离单条文本记录的
/// trailing entry：先通用条目（置位位数即个数），后 multibyte overlap
/// （末字节低 2 位 + 1）。编码为 0 时保守停剥（规范上不会出现 0 大小）。
fn strip_trailing_data(mut data: &[u8], flags: u16) -> &[u8] {
    let mut trailers = 0usize;
    let mut f = flags;
    while f > 1 {
        if f & 2 != 0 {
            trailers += 1;
        }
        f >>= 1;
    }
    for _ in 0..trailers {
        let num = trailing_entry_size(data);
        if num == 0 {
            break;
        }
        if num >= data.len() {
            return &[];
        }
        data = &data[..data.len() - num];
    }
    if flags & 1 != 0 && !data.is_empty() {
        let num = ((data[data.len() - 1] & 3) as usize) + 1;
        if num < data.len() {
            data = &data[..data.len() - num];
        }
    }
    data
}

/// PalmDoc LZ77 解压（含输出上界）。
///
/// 吸收自 ebook-rs 0.18.0 `src/mobi.rs::decompress_palmdoc`（MIT）：
/// 单输入字节最多膨胀 8 倍，`MAX_PALMDOC_OUTPUT` 截断防炸内存；
/// LZ77 距离环回语义按原实现保留。
fn decompress_palmdoc(data: &[u8]) -> Vec<u8> {
    const MAX_PALMDOC_OUTPUT: usize = 256 * 1024 * 1024;
    let init_cap = data.len().saturating_mul(2).min(8 * 1024 * 1024);
    let mut out = Vec::with_capacity(init_cap);
    let mut i = 0;

    while i < data.len() {
        if out.len() >= MAX_PALMDOC_OUTPUT {
            break;
        }
        let byte = data[i];
        i += 1;

        match byte {
            0x00 | 0x09..=0x7F => {
                out.push(byte);
            }
            0x01..=0x08 => {
                let count = byte as usize;
                let end = (i + count).min(data.len());
                out.extend_from_slice(&data[i..end]);
                i += count;
            }
            0x80..=0xBF => {
                if i < data.len() {
                    let next = data[i];
                    i += 1;
                    let distance = (((byte & 0x3F) as usize) << 5) | ((next >> 3) as usize);
                    let length = (next & 0x07) as usize + 3;

                    if distance > 0 && distance <= out.len() {
                        let start = out.len() - distance;
                        for j in 0..length {
                            let val = out[start + (j % distance)];
                            out.push(val);
                        }
                    } else {
                        out.push(byte);
                        out.push(next);
                    }
                } else {
                    out.push(byte);
                }
            }
            0xC0..=0xFF => {
                out.push(b' ');
                out.push(byte ^ 0x80);
            }
        }
    }

    out
}

// ── PalmDB 容器 ─────────────────────────────────────────────────────

struct Pdb<'a> {
    /// 各记录起始偏移；记录 i 数据 = [offset[i], offset[i+1] | 文件尾)
    offsets: Vec<u32>,
    bytes: &'a [u8],
}

impl<'a> Pdb<'a> {
    /// 解析 PDB 头 + 记录表。结构性失败返回 `None`——此时无法判定是否
    /// KF8，调用方回落 mobi crate 通路让其给出准确的 corrupt 错误。
    fn parse(bytes: &'a [u8]) -> Option<Pdb<'a>> {
        if bytes.len() < 78 {
            return None;
        }
        let num_records = rd_u16(bytes, 76)? as usize;
        if num_records == 0 || bytes.len() < 78 + num_records * 8 {
            return None;
        }
        let mut offsets = Vec::with_capacity(num_records);
        for i in 0..num_records {
            offsets.push(rd_u32(bytes, 78 + i * 8)?);
        }
        Some(Pdb { offsets, bytes })
    }

    fn len(&self) -> usize {
        self.offsets.len()
    }

    fn record(&self, i: usize) -> Option<&'a [u8]> {
        let start = *self.offsets.get(i)? as usize;
        let end = self
            .offsets
            .get(i + 1)
            .map_or(self.bytes.len(), |&e| e as usize);
        if start > end || end > self.bytes.len() {
            return None;
        }
        Some(&self.bytes[start..end])
    }

    /// PalmDB 名（文件头前 32 字节，去零填充与首尾空白）
    fn file_name(&self) -> String {
        let head = &self.bytes[..self.bytes.len().min(32)];
        String::from_utf8_lossy(head)
            .trim_end_matches('\0')
            .trim()
            .to_string()
    }
}

// ── 记录 0（PalmDoc 头 + MOBI 头 + EXTH）────────────────────────────

/// 记录 0 字段视图。偏移为记录内绝对偏移（PalmDoc 头 16B + "MOBI" 4B
/// + header_length 4B 之后按 MOBI 头字段序排布，与 mobi-0.8.0 mobih.rs
///   读取序对齐），越界字段读作缺省。
struct Rec0<'a> {
    raw: &'a [u8],
    compression: u16,
    text_record_count: u16,
    encryption: u16,
    /// raw[16..20] == b"MOBI"
    has_mobi: bool,
    header_len: u32,
    format_version: u32,
    name_offset: u32,
    name_length: u32,
    locale: u8,
    drm_offset: u32,
    /// EXTH 记录 (type, data)
    exth: Vec<(u32, &'a [u8])>,
}

impl<'a> Rec0<'a> {
    fn parse(raw: &'a [u8]) -> Rec0<'a> {
        // PalmDoc 头 16B：compression@0、text_length@4、record_count@8、
        // record_size@10、encryption@12
        let compression = rd_u16(raw, 0).unwrap_or(0);
        let text_record_count = rd_u16(raw, 8).unwrap_or(0);
        let encryption = rd_u16(raw, 12).unwrap_or(0);
        let has_mobi = raw.len() >= 20 && &raw[16..20] == b"MOBI";
        let header_len = rd_u32(raw, 20).unwrap_or(0);
        let format_version = rd_u32(raw, 104).unwrap_or(0);
        let name_offset = rd_u32(raw, 84).unwrap_or(0);
        let name_length = rd_u32(raw, 88).unwrap_or(0);
        let locale = *raw.get(94).unwrap_or(&0);
        let drm_offset = rd_u32(raw, 168).unwrap_or(0);
        let exth = Self::walk_exth(raw, header_len);
        Rec0 {
            raw,
            compression,
            text_record_count,
            encryption,
            has_mobi,
            header_len,
            format_version,
            name_offset,
            name_length,
            locale,
            drm_offset,
            exth,
        }
    }

    /// EXTH 遍历（exth_flags 0x40 位 = 有 EXTH，头在 16 + header_len；
    /// 记录 = type u32 + len u32 含 8 字节头；坏长度保守截断）。
    fn walk_exth(raw: &'a [u8], header_len: u32) -> Vec<(u32, &'a [u8])> {
        let mut out = Vec::new();
        if rd_u32(raw, 128).unwrap_or(0) & 0x40 == 0 {
            return out;
        }
        let Some(base) = 16usize.checked_add(header_len as usize) else {
            return out;
        };
        if base + 12 > raw.len() || &raw[base..base + 4] != b"EXTH" {
            return out;
        }
        let count = rd_u32(raw, base + 8).unwrap_or(0);
        let mut cur = base + 12;
        for _ in 0..count {
            let (Some(ty), Some(len)) = (rd_u32(raw, cur), rd_u32(raw, cur + 4)) else {
                break;
            };
            if len < 8 {
                break;
            }
            let Some(end) = cur.checked_add(len as usize).filter(|&e| e <= raw.len()) else {
                break;
            };
            out.push((ty, &raw[cur + 8..end]));
            cur = end;
        }
        out
    }

    fn exth_first_u32(&self, ty: u32) -> Option<u32> {
        let (_, d) = self.exth.iter().find(|(t, d)| *t == ty && d.len() >= 4)?;
        rd_u32(d, 0)
    }

    fn exth_all_strings(&self, ty: u32) -> Vec<String> {
        self.exth
            .iter()
            .filter(|(t, _)| *t == ty)
            .map(|(_, d)| String::from_utf8_lossy(d).trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn exth_first_string(&self, ty: u32) -> Option<String> {
        self.exth_all_strings(ty).into_iter().next()
    }

    /// 全名（name_offset 相对记录 0 起点，去零填充与首尾空白）
    fn full_name(&self) -> String {
        let off = self.name_offset as usize;
        let len = self.name_length as usize;
        if len == 0 || off.checked_add(len).is_none_or(|e| e > self.raw.len()) {
            return String::new();
        }
        String::from_utf8_lossy(&self.raw[off..off + len])
            .trim_end_matches('\0')
            .trim()
            .to_string()
    }
}

// ── INDX 索引（SKEL/FRAG 共用的最小子集）────────────────────────────

/// INDX 条目：label（SKEL=骨架名 / FRAG=十进制插入位置）+ tag 值表。
struct IndxEntry {
    label: String,
    tags: HashMap<u8, Vec<u64>>,
}

/// 解析一个 INDX 索引链（主记录 + count 个条目记录）。
///
/// 结构：主记录 "INDX" + 13×u32 头（len@4、start@20、count@24），
/// TAGX 段在 `len` 偏移（'TAGX' + firstEntryOffset + controlByteCount +
/// 4 字节条目 [tag, valuesPerEntry, mask, endFlag]）；条目记录同头形，
/// IDXT 段在其头 `start` 偏移（4B 魔数 + u16×count 条目起点，末条止于
/// IDXT 自身）。
fn parse_indx(pdb: &Pdb, idx: usize) -> Result<Vec<IndxEntry>> {
    let main = pdb
        .record(idx)
        .ok_or_else(|| DocError::corrupt("KF8 索引主记录越界"))?;
    if main.len() < 56 || &main[0..4] != b"INDX" {
        return corrupt("KF8 索引主记录缺 INDX 头");
    }
    let header_len = rd_u32(main, 4).unwrap_or(0) as usize;
    let record_count = rd_u32(main, 24).unwrap_or(0);
    // 条目记录数上界防御（真实 SKEL/FRAG 均为个位数）
    if record_count > 4096 {
        return corrupt("KF8 索引条目记录数异常");
    }

    // TAGX
    let tagx = header_len;
    if tagx + 12 > main.len() || &main[tagx..tagx + 4] != b"TAGX" {
        return corrupt("KF8 索引缺 TAGX 段");
    }
    let first_entry = rd_u32(main, tagx + 4).unwrap_or(0) as usize;
    let cbc = rd_u32(main, tagx + 8).unwrap_or(0) as usize;
    let mut tags: Vec<(u8, u8, u8, u8)> = Vec::new();
    let mut ti = tagx + 12;
    while ti + 4 <= main.len() && ti < tagx + first_entry {
        tags.push((main[ti], main[ti + 1], main[ti + 2], main[ti + 3]));
        ti += 4;
    }

    let mut entries = Vec::new();
    for k in 0..record_count as usize {
        let rec = pdb
            .record(idx + 1 + k)
            .ok_or_else(|| DocError::corrupt("KF8 索引条目记录越界"))?;
        if rec.len() < 56 || &rec[0..4] != b"INDX" {
            return corrupt("KF8 索引条目记录缺 INDX 头");
        }
        let idxt = rd_u32(rec, 20).unwrap_or(0) as usize;
        let entry_count = rd_u32(rec, 24).unwrap_or(0);
        if entry_count > 65535 || idxt + 4 > rec.len() {
            return corrupt("KF8 索引 IDXT 段越界");
        }
        let mut starts = Vec::with_capacity(entry_count as usize + 1);
        for j in 0..entry_count as usize {
            let Some(p) = rd_u16(rec, idxt + 4 + j * 2) else {
                return corrupt("KF8 索引 IDXT 条目越界");
            };
            starts.push(p as usize);
        }
        starts.push(idxt);
        for w in 0..entry_count as usize {
            let (Some(&s), Some(&e)) = (starts.get(w), starts.get(w + 1)) else {
                continue;
            };
            if s < rec.len().min(e) && s < e {
                let tlen = rec[s] as usize;
                if s + 1 + tlen > e {
                    return corrupt("KF8 索引条目 label 越界");
                }
                let label = String::from_utf8_lossy(&rec[s + 1..s + 1 + tlen]).into_owned();
                let tag_map = read_tag_map(rec, s + 1 + tlen, cbc, &tags)?;
                entries.push(IndxEntry {
                    label,
                    tags: tag_map,
                });
            }
        }
    }
    Ok(entries)
}

/// 条目 tag 值解码：控制字节按 TAGX 掩码位提示各 tag 的值个数（掩码
/// 全置位且多于一位时，数据区先跟一个变长"字节长度"，长度内是连续
/// 变长值；掩码部分置位时，位值即个数），值均为变长 7 位编码。
fn read_tag_map(
    data: &[u8],
    entry_start: usize,
    cbc: usize,
    tags: &[(u8, u8, u8, u8)],
) -> Result<HashMap<u8, Vec<u64>>> {
    let mut out: HashMap<u8, Vec<u64>> = HashMap::new();
    let mut cb_index = 0usize;
    let mut cursor = entry_start + cbc;
    // (tag, valuesPerEntry, count 或 byteLength)
    let mut pending: Vec<(u8, u8, Option<u64>, Option<u64>)> = Vec::new();
    for &(tag, vpe, mask, end_flag) in tags {
        if end_flag == 0x01 {
            cb_index += 1;
            continue;
        }
        let Some(&cbyte) = data.get(entry_start + cb_index) else {
            return corrupt("KF8 索引控制字节越界");
        };
        let mut masked = (cbyte & mask) as u32;
        if masked == 0 {
            continue;
        }
        if masked == mask as u32 {
            if mask.count_ones() > 1 {
                let (consumed, n) = read_var_width(data, cursor)
                    .ok_or_else(|| DocError::corrupt("KF8 索引变长值越界"))?;
                cursor += consumed;
                pending.push((tag, vpe, None, Some(n)));
            } else {
                pending.push((tag, vpe, Some(1), None));
            }
        } else {
            let mut m = mask;
            while m & 0x01 == 0 {
                m >>= 1;
                masked >>= 1;
            }
            pending.push((tag, vpe, Some(u64::from(masked)), None));
        }
    }
    for (tag, vpe, count, byte_len) in pending {
        let mut values = Vec::new();
        match (count, byte_len) {
            (Some(c), _) => {
                for _ in 0..c {
                    for _ in 0..vpe {
                        let (consumed, v) = read_var_width(data, cursor)
                            .ok_or_else(|| DocError::corrupt("KF8 索引 tag 值越界"))?;
                        cursor += consumed;
                        values.push(v);
                    }
                }
            }
            (None, Some(n)) => {
                let mut total = 0u64;
                while total < n {
                    let (consumed, v) = read_var_width(data, cursor)
                        .ok_or_else(|| DocError::corrupt("KF8 索引 tag 值越界"))?;
                    cursor += consumed;
                    total += consumed as u64;
                    values.push(v);
                }
            }
            (None, None) => continue,
        }
        out.insert(tag, values);
    }
    Ok(out)
}

// ── FDST / 装配 ─────────────────────────────────────────────────────

/// FDST 记录切表：'FDST' 魔数 + 表项组数@8；每组 (start, end) u32 自
/// 12 起，取各组 start（末尾补 rawML 长度）。坏表返回 None（保守取
/// 整个 rawML 为 flow 0）。
fn parse_fdst(rec: &[u8], raw_ml_len: usize) -> Option<Vec<usize>> {
    if rec.len() < 12 || &rec[0..4] != b"FDST" {
        return None;
    }
    let n = rd_u32(rec, 8)? as usize;
    if n == 0 || n > 4096 {
        return None;
    }
    let mut starts = Vec::with_capacity(n + 1);
    for i in 0..n {
        starts.push(rd_u32(rec, 12 + i * 8)? as usize);
    }
    starts.push(raw_ml_len);
    Some(starts)
}

struct SkelEntry {
    frag_cnt: usize,
    /// skeleton 在 flow 0 中的起点（绝对 rawML 偏移）
    start: usize,
    len: usize,
}

struct FragEntry {
    /// 插入位置（rawML 绝对偏移；装配时减去所在 skeleton 起点）
    insert_pos: usize,
    length: usize,
}

/// skeleton/fragment 装配（独立函数便于单测）：每个 skeleton 是一个
/// XHTML part 骨架，其 frag_cnt 个 fragment 按 rawML 中的紧邻顺序逐一
/// 插回各自 insert 位置；insert 位置越界时钳制到骨架当前尾（防御畸形
/// 索引）。
fn assemble_parts(flow0: &[u8], skels: &[SkelEntry], frags: &[FragEntry]) -> Result<Vec<u8>> {
    let mut parts: Vec<u8> = Vec::new();
    let mut frag_ptr = 0usize;
    for sk in skels {
        let Some(sk_end) = sk.start.checked_add(sk.len).filter(|&e| e <= flow0.len()) else {
            return corrupt("KF8 skeleton 区间越界");
        };
        let mut buf = flow0[sk.start..sk_end].to_vec();
        let mut base = sk_end;
        for _ in 0..sk.frag_cnt {
            let Some(f) = frags.get(frag_ptr) else {
                return corrupt("KF8 fragment 表不足");
            };
            let Some(f_end) = base.checked_add(f.length).filter(|&e| e <= flow0.len()) else {
                return corrupt("KF8 fragment 区间越界");
            };
            let rel = f.insert_pos.saturating_sub(sk.start).min(buf.len());
            let mut merged = Vec::with_capacity(buf.len() + f.length);
            merged.extend_from_slice(&buf[..rel]);
            merged.extend_from_slice(&flow0[base..f_end]);
            merged.extend_from_slice(&buf[rel..]);
            buf = merged;
            base = f_end;
            frag_ptr += 1;
        }
        parts.extend_from_slice(&buf);
    }
    Ok(parts)
}

/// 文本区控制字符消毒：保留 \n\r\t（filepos 残留/畸形字节对块化无意义）。
fn sanitize_control_chars(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .copied()
        .filter(|&c| c == b'\n' || c == b'\r' || c == b'\t' || c >= 0x20)
        .collect()
}

/// locale 字节 → 语言名小写（复用 mobi crate 语言表，与 MOBI7 通路
/// `language_of` 的 Debug 小写口径一致；Neutral/Unknown 记空）。
fn locale_language(locale: u8) -> String {
    match format!("{:?}", Language::from(locale)).to_ascii_lowercase().as_str() {
        "unknown" | "neutral" | "" => String::new(),
        s => s.to_string(),
    }
}

// ── 入口 ────────────────────────────────────────────────────────────

/// 侦测并解析 KF8（azw3）。返回：
/// * `Ok(None)`——不是 KF8（PDB 读不出 / 无 EXTH 121 boundary 且
///   `format_version < 8`），调用方回落 mobi crate 的 MOBI7 通路；
/// * `Ok(Some(raw))`——解析成功；
/// * `Err`——已识别为 KF8 但解析失败：corrupt 或 encrypted（终止码，
///   不回落，防止混合件静默降级到 MOBI7 旧正文）。
pub(super) fn try_parse(bytes: &[u8]) -> Result<Option<RawDoc>> {
    let Some(pdb) = Pdb::parse(bytes) else {
        return Ok(None);
    };
    let rec0 = match pdb.record(0) {
        Some(r) => r,
        None => return Ok(None),
    };
    let head = Rec0::parse(rec0);

    // KF8 基准记录：EXTH 121（KF8 Boundary）→ 混合件剥离取 KF8 部分；
    // 无 boundary 但 format_version == 8 → 纯 KF8
    let base = match head.exth_first_u32(121) {
        Some(b) if b > 0 && (b as usize) < pdb.len() => b as usize,
        _ => {
            if head.format_version == 8 {
                0
            } else {
                return Ok(None);
            }
        }
    };

    let kf8_raw = pdb
        .record(base)
        .ok_or_else(|| DocError::corrupt("KF8 头记录越界"))?;
    let kf8 = Rec0::parse(kf8_raw);
    if !kf8.has_mobi || kf8.format_version != 8 {
        return corrupt("EXTH 121 未指向 KF8 头（format_version != 8）");
    }
    // KF8 头（header_length 字段真实值 = 0xF8）必须覆盖扩展字段区
    // （frag_idx@248 / skel_idx@252；EXTH 在 16 + header_length 之后）
    if kf8.header_len < 236 {
        return corrupt("KF8 头过短，缺 skeleton/fragment 索引字段");
    }

    // DRM/加密是终止码：PalmDoc encryption 字段 + drm_offset（与 MOBI7
    // 通路同款双判定）
    if kf8.encryption != 0 || kf8.drm_offset != 0xFFFF_FFFF {
        return Err(DocError::encrypted("KF8 带 DRM 加密，不支持解密"));
    }

    // 压缩类型：1=无 2=PalmDoc；17480=HUFF/CDIC（KF7 独有）
    match kf8.compression {
        1 | 2 => {}
        other => {
            return corrupt(format!(
                "KF8 正文压缩类型不支持: {other}（HUFF/CDIC 为 MOBI7 独有）"
            ))
        }
    }

    // rawML：记录 base+1..=base+text_record_count 各自剥 trailing 后解压拼接
    let n = kf8.text_record_count as usize;
    if n == 0 || base + n >= pdb.len() {
        return corrupt("KF8 文本记录数为 0 或越界");
    }
    let extra_flags = rd_u16(kf8_raw, 242).unwrap_or(0);
    let mut raw_ml = Vec::new();
    for i in 1..=n {
        let Some(rec) = pdb.record(base + i) else {
            return corrupt("KF8 文本记录越界");
        };
        let stripped = strip_trailing_data(rec, extra_flags);
        let chunk = if kf8.compression == 2 {
            decompress_palmdoc(stripped)
        } else {
            stripped.to_vec()
        };
        raw_ml.extend_from_slice(&chunk);
    }

    // FDST 切 flow（flow 0 = 正文区；坏表保守取整个 rawML）
    let flow0: &[u8] = {
        let fdst_rec_no = rd_u32(kf8_raw, 192).unwrap_or(0xFFFF_FFFF);
        let fdst_cnt = rd_u32(kf8_raw, 196).unwrap_or(0);
        let table = usize::try_from(fdst_rec_no)
            .ok()
            .filter(|_| fdst_cnt > 1)
            .and_then(|fno| pdb.record(fno))
            .and_then(|rec| parse_fdst(rec, raw_ml.len()));
        match table {
            Some(tbl) if tbl.len() >= 2 => {
                let s0 = tbl[0].min(raw_ml.len());
                let s1 = tbl[1].min(raw_ml.len());
                if s0 < s1 {
                    &raw_ml[s0..s1]
                } else {
                    &raw_ml[..]
                }
            }
            _ => &raw_ml[..],
        }
    };

    // SKEL/FRAG 索引（记录内偏移 248/252；值为相对 KF8 部分起始的记录号）
    let skel_idx = rd_u32(kf8_raw, 252).unwrap_or(0xFFFF_FFFF);
    let frag_idx = rd_u32(kf8_raw, 248).unwrap_or(0xFFFF_FFFF);
    if skel_idx == 0xFFFF_FFFF || frag_idx == 0xFFFF_FFFF {
        return corrupt("KF8 头缺 skeleton/fragment 索引（header_length 过短或非正文 KF8）");
    }
    let abs = |v: u32| -> Option<usize> {
        usize::try_from(v)
            .ok()
            .and_then(|v| v.checked_add(base))
            .filter(|&v| v < pdb.len())
    };
    let (Some(skel_abs), Some(frag_abs)) = (abs(skel_idx), abs(frag_idx)) else {
        return corrupt("KF8 索引记录号越界");
    };

    // SKEL 条目：tag1 = fragment 个数，tag6 = [flow0 起点, 骨架长度]
    let mut skels = Vec::new();
    for e in parse_indx(&pdb, skel_abs)? {
        let (Some(c), Some(s), Some(l)) = (
            e.tags.get(&1).and_then(|v| v.first()).copied(),
            e.tags.get(&6).and_then(|v| v.first()).copied(),
            e.tags.get(&6).and_then(|v| v.get(1)).copied(),
        ) else {
            return corrupt("KF8 skeleton 条目缺 tag 值");
        };
        let (Some(c), Some(s), Some(l)) = (
            usize::try_from(c).ok(),
            usize::try_from(s).ok(),
            usize::try_from(l).ok(),
        ) else {
            return corrupt("KF8 skeleton 条目值越界");
        };
        skels.push(SkelEntry {
            frag_cnt: c,
            start: s,
            len: l,
        });
    }
    // FRAG 条目：label = 十进制插入位置，tag6 = [rawML 起点, 长度]
    let mut frags = Vec::new();
    for e in parse_indx(&pdb, frag_abs)? {
        let Ok(ip) = e.label.trim().parse::<usize>() else {
            return corrupt("KF8 fragment 插入位置非法");
        };
        let Some(l) = e.tags.get(&6).and_then(|v| v.get(1)).copied() else {
            return corrupt("KF8 fragment 条目缺 tag 值");
        };
        let Ok(l) = usize::try_from(l) else {
            return corrupt("KF8 fragment 长度越界");
        };
        frags.push(FragEntry {
            insert_pos: ip,
            length: l,
        });
    }

    if skels.is_empty() {
        return corrupt("KF8 skeleton 表为空");
    }
    let assembled = assemble_parts(flow0, &skels, &frags)?;
    let assembled = sanitize_control_chars(&assembled);
    let html = String::from_utf8_lossy(&assembled).into_owned();
    if html.trim().is_empty() {
        return corrupt("KF8 正文为空（skeleton/fragment 装配结果为空）");
    }

    // ── 元数据（与 MOBI7 通路同口径，取 KF8 头 EXTH）──
    let mut meta = HashMap::new();
    // 书名：EXTH 503 → 全名 → PalmDB 名
    let title = kf8
        .exth_first_string(503)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let n = kf8.full_name();
            if n.is_empty() {
                Some(pdb.file_name())
            } else {
                Some(n)
            }
        });
    if let Some(title) = title.filter(|s| !s.is_empty()) {
        meta.insert("mobi.title".to_string(), title);
    }
    let authors = kf8.exth_all_strings(100).join("; ");
    if !authors.is_empty() {
        meta.insert("mobi.authors".to_string(), authors);
    }
    let language = kf8
        .exth_first_string(524)
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| locale_language(kf8.locale));
    if !language.is_empty() {
        meta.insert("mobi.language".to_string(), language);
    }

    Ok(Some(RawDoc {
        pages: vec![RawPage {
            items: vec![RawTextItem {
                text: html,
                ..Default::default()
            }],
            ..Default::default()
        }],
        meta,
        needs_ocr: false,
    }))
}

// ── 单测：最小字节级件（格式常量/解码/装配语义）────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 通用 trailing entry：条目末字节 = 0x80|大小（高位字节在扫描窗口
    /// 内重置累积器，条目大小即其低 7 位）。
    #[test]
    fn strips_generic_trailing_entries() {
        // 2 字节条目：填充字节 + 末字节 0x82
        let rec = b"<p>hi</p>\x00\x82";
        assert_eq!(trailing_entry_size(&rec[..]), 2);
        assert_eq!(strip_trailing_data(rec, 0b10), b"<p>hi</p>");
        // 多个通用条目：两个 1 字节条目（0x81）背靠背
        let rec = b"abc\x81\x81";
        assert_eq!(strip_trailing_data(rec, 0b110), b"abc");
    }

    /// multibyte overlap 条目：末字节低 2 位 + 1。
    #[test]
    fn strips_multibyte_overlap_trailing_entry() {
        // (0x01 & 3) + 1 = 2 → 连同前一个字节一起剥掉
        let rec = b"body\x41\x01";
        assert_eq!(strip_trailing_data(rec, 0b1), b"body");
        // 通用条目先剥（physically 在最后），multibyte 数据在后剥：
        // 记录尾 = 正文 + multibyte(2B) + 通用末字节 0x81
        let rec = b"body\x41\x01\x81";
        assert_eq!(strip_trailing_data(rec, 0b11), b"body");
    }

    /// 反向 7 位变长编码：高位置 1 终止（单字节值也须置位，如 5 → [0x85]）。
    #[test]
    fn var_width_decode() {
        assert_eq!(read_var_width(&[0x85], 0), Some((1, 5)));
        assert_eq!(read_var_width(&[0x80], 0), Some((1, 0)));
        // 多字节：高位组在前（无高位），末字节（最低组）高位置 1 终止
        assert_eq!(read_var_width(&[0x05, 0x82], 0), Some((2, 642)));
        assert_eq!(read_var_width(&[0x01, 0x80], 0), Some((2, 128)));
        // 未终止（末字节高位未置 1）→ 数据耗尽返回 None
        assert_eq!(read_var_width(&[0x05], 0), None);
    }

    /// tag 值解码：部分置位掩码 = 位值即个数；全置位多 bit 掩码 =
    /// 先变长字节长度再读值。
    #[test]
    fn tag_map_decode() {
        // 条目：控制字节 + 值区（值为变长 7 位编码）。tags: tag1(1 值/条,
        // 0x02), tag6(2 值/条, 0x04)，末位 tag0 是控制字节边界标记
        // （endFlag=1，不携带值）
        let tags = [
            (1u8, 1u8, 0x02u8, 0u8),
            (6, 2, 0x04, 0x00),
            (0, 0, 0x08, 0x01),
        ];
        // cb=0x06，值：vwi(3)=[0x83]；vwi(0)=[0x80]、vwi(45)=[0xad]
        let entry = [0x06u8, 0x83, 0x80, 0xad];
        let map = read_tag_map(&entry, 0, 1, &tags).unwrap();
        assert_eq!(map.get(&1), Some(&vec![3u64]));
        assert_eq!(map.get(&6), Some(&vec![0u64, 45]));
        // 全置位多 bit 掩码：0x18（两个 bit）→ 先读字节长度 vwi(2)=[0x82]，
        // 再读 2 字节值 vwi(7)=[0x87]、vwi(1)=[0x81]
        let tags = [(9u8, 1u8, 0x18u8, 0u8), (0, 0, 0x20, 0x01)];
        let entry = [0x18u8, 0x82, 0x87, 0x81];
        let map = read_tag_map(&entry, 0, 1, &tags).unwrap();
        assert_eq!(map.get(&9), Some(&vec![7u64, 1u64]));
    }

    /// skeleton/fragment 装配：两骨架两片段按插入位置归位。
    #[test]
    fn assembles_skeletons_and_fragments() {
        let sk0 = b"<html><body><h1>A</h1>";
        let sk1 = b"<h2>B</h2></body></html>";
        let f0 = b"<p>p1</p>";
        let f1 = b"<p>p2</p>";
        let mut flow0 = Vec::new();
        flow0.extend_from_slice(sk0);
        flow0.extend_from_slice(f0);
        flow0.extend_from_slice(sk1);
        flow0.extend_from_slice(f1);
        let skels = vec![
            SkelEntry {
                frag_cnt: 1,
                start: 0,
                len: sk0.len(),
            },
            SkelEntry {
                frag_cnt: 1,
                start: sk0.len() + f0.len(),
                len: sk1.len(),
            },
        ];
        let frags = vec![
            FragEntry {
                insert_pos: sk0.len(),
                length: f0.len(),
            },
            FragEntry {
                // 第二骨架内 `</h2>` 之后的绝对 rawML 位置
                insert_pos: sk0.len() + f0.len() + sk1.len() - "</body></html>".len(),
                length: f1.len(),
            },
        ];
        let out = assemble_parts(&flow0, &skels, &frags).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "<html><body><h1>A</h1><p>p1</p><h2>B</h2><p>p2</p></body></html>"
        );
    }

    /// FDST 切表 + flow 0 区间。
    #[test]
    fn fdst_table_and_flow0() {
        let mut rec = Vec::new();
        rec.extend_from_slice(b"FDST");
        rec.extend_from_slice(&0u32.to_be_bytes()); // 保留位
        rec.extend_from_slice(&2u32.to_be_bytes()); // 2 组
        rec.extend_from_slice(&0u32.to_be_bytes());
        rec.extend_from_slice(&10u32.to_be_bytes());
        rec.extend_from_slice(&10u32.to_be_bytes());
        rec.extend_from_slice(&20u32.to_be_bytes());
        let tbl = parse_fdst(&rec, 20).unwrap();
        assert_eq!(tbl, vec![0, 10, 20]);

        let raw: Vec<u8> = (0..20).collect();
        let flow0 = &raw[tbl[0]..tbl[1]];
        assert_eq!(flow0.len(), 10);
    }

    /// locale 回退表与 MOBI7 通路同口径。
    #[test]
    fn locale_language_fallback() {
        assert_eq!(locale_language(9), "english");
        assert_eq!(locale_language(4), "chinese");
        assert_eq!(locale_language(0), "");
    }

    /// EXTH 121 boundary 侦测（最小 KF8 记录 0：header_length = 0xF8，
    /// 扩展字段在头内、EXTH 在 16+248=264 处）。
    #[test]
    fn boundary_detection() {
        let mut rec0 = Vec::new();
        rec0.extend_from_slice(&1u16.to_be_bytes()); // compression
        rec0.extend_from_slice(&0u16.to_be_bytes()); // unused
        rec0.extend_from_slice(&10u32.to_be_bytes()); // text_length
        rec0.extend_from_slice(&1u16.to_be_bytes()); // record_count
        rec0.extend_from_slice(&4096u16.to_be_bytes()); // record_size
        rec0.extend_from_slice(&0u16.to_be_bytes()); // encryption
        rec0.extend_from_slice(&0u16.to_be_bytes()); // unused
        rec0.extend_from_slice(b"MOBI");
        rec0.extend_from_slice(&248u32.to_be_bytes()); // header_length（KF8 = 0xF8）
        let mut body = vec![0u8; 240];
        // format_version @104（body 内 80）、exth_flags @128（body 内 104）、
        // frag_idx @248（body 内 224）、skel_idx @252（body 内 228）
        body[4..8].copy_from_slice(&65001u32.to_be_bytes());
        body[80..84].copy_from_slice(&8u32.to_be_bytes());
        body[104..108].copy_from_slice(&0x40u32.to_be_bytes());
        body[224..228].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        body[228..232].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        rec0.extend_from_slice(&body);
        // EXTH：1 条记录 121 = 7
        let mut exth = Vec::new();
        exth.extend_from_slice(&121u32.to_be_bytes());
        exth.extend_from_slice(&12u32.to_be_bytes());
        exth.extend_from_slice(&7u32.to_be_bytes());
        let mut hdr = Vec::new();
        hdr.extend_from_slice(b"EXTH");
        hdr.extend_from_slice(&(12 + exth.len() as u32).to_be_bytes());
        hdr.extend_from_slice(&1u32.to_be_bytes());
        rec0.extend_from_slice(&hdr);
        rec0.extend_from_slice(&exth);

        let head = Rec0::parse(&rec0);
        assert_eq!(head.format_version, 8);
        assert_eq!(head.header_len, 248);
        assert_eq!(head.exth_first_u32(121), Some(7));
        // 全名/EXTH 复合：503 记录取字符串
        assert_eq!(head.exth_first_string(503), None);
    }
}
