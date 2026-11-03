//! MOBI 后端（Kindle 电子书 / Mobipocket，feature = "mobi"，docparse-plan §7 M5；
//! azw3/KF8 为用户需求增强轮）。
//!
//! 支持面：
//! * **MOBI7（含 kindlegen 混合件的 MOBI7 部分）走 mobi crate 0.8.0** 依源码
//!   核实（registry 缓存 `~/.cargo/registry/src/mirrors.ustc.edu.cn-38d0e5eb5da2abae/mobi-0.8.0/`）：
//!   `Mobi::from_read`（src/lib.rs L94-107）打开 → `title()/author()/language()`
//!   等元数据访问（L130-177，EXTH 记录 → 缺省回落）→
//!   `content_as_string_lossy()`（L321-327）按压缩类型（无压缩/PalmDoc/HUFF，
//!   palmdoch.rs L20-28）解压 readable 记录区间（L216-219，
//!   `first_content_record..first_non_book_index`）拼出**整篇 HTML**。
//! * **azw3/KF8 走自研解析（子模块 `kf8`）**：mobi crate 对 KF8 文件
//!   **打开不报错但正文是乱码**（`MobiMetadata::from_reader` 对记录 0 无版本
//!   校验，headers/mod.rs L60-128；KF8 记录是 RESC/SKELETON/fragment 结构且
//!   每条带 trailing bytes，crate 无对应处理）——比报错更糟，故 azw3 必须
//!   自研。混合件（MOBI7+KF8 同包）按 **EXTH 121 boundary** 剥离取 KF8 部分
//!   （翻译场景新格式优先；取舍详见 kf8 模块头）。
//! * 已知缺陷族（mobi crate）：① `MobiType` 枚举无 KF8 变体、`format_version`
//!   （KF8=8）不公开（mobih.rs L22-63/L153）；② PalmDOC 解压不做逐记录
//!   trailing-entry 剥离（`extra_record_data_flags`，mobih.rs L317 解析后从未
//!   消费）；③ `RawRecords::range` 双重差一（record.rs L88：`end = (b-1).min(len-1)`，
//!   任何 range 都覆盖不到末条记录，小书会坍缩为空集；真实书籍记录数远大于
//!   区间边界、不受影响，构造件以尾部哨兵记录绕过）。KF8 支路不依赖该 crate，
//!   三项缺陷均不受累。
//!
//! 数据流（epub.rs 同款"原文整体交插件"模式）：
//! `MobiRawBackend`（ParseCapability）→ 元数据进 `RawDoc.meta`
//! （`mobi.title` / `mobi.authors` / `mobi.language`，KF8 与 MOBI7 同键名）
//! + 整篇 HTML 进 `items[0]` → `MobiPlugin`（FormatPlugin）`assemble` 用
//!   `plugins::html::parse_blocks` 块化为单页结构化文档。
//!
//! 吸收/参考来源（合规详情见 kf8 模块头与 THIRD_PARTY_NOTICES.md）：
//! * document-parser 0.1.0-alpha.18 的 mobi loader（vendor/document-parser-
//!   0.1.0-alpha.18/src/formats/mobi/loader.rs）仅作模式参考——语言规范化口径
//!   （枚举 Debug 小写，其 loader.rs L164-174）与"整篇 HTML 交上层"；其块化用
//!   scraper（html5ever 全家桶，docparse-plan §2.4 明确不引入），本模块改喂
//!   自有 HTML 扫描器。
//! * KF8 支路：PalmDoc 解压例程吸收自 ebook-rs 0.18.0（MIT）；记录语义依
//!   KindleUnpack 家族（GPL-3.0）格式规范参考、独立实现（不吸收代码）。
//! * mobi crate（MIT，crates.io 存档）为 MOBI7 通路直接依赖，feature `time`
//!   （chrono）不开。
//!
//! DRM：encrypted 是终止码不回退（docparse-plan §6）——MOBI7：PalmDoc 头
//! encryption 字段或 MOBI 头 drm_offset 任一命中即拒绝；KF8：同款双判定，
//! 且已识别为 KF8 后失败不回落 mobi crate（防混合件静默降级）。不做解密。
//!
//! 测试件：mobi-0.8.0 存档无 tests/ 目录与 .mobi/.azw3 样例（核实于
//! registry 缓存），故构造最小 MOBI7/KF8 二进制金测（手工编字节；MOBI7
//! 构造法与 mobi-rs 自带 src/book.rs 同源，KF8 按 kf8 模块头语义构造，
//! 含 EXTH 121 boundary、SKEL/FRAG INDX、FDST、multibyte trailing），
//! 另留 `#[ignore]` 真件集成测试位。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use mobi::headers::{Encryption, ExthRecord};
use mobi::Mobi;

use crate::error::{DocError, Result};
use crate::model::{Document, Page};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawPage, RawTextItem};
use crate::plugins::html::parse_blocks;
use crate::registry::Registry;

mod kf8;

// ── 元数据归一 ──────────────────────────────────────────────────────

/// 书名归一：EXTH 503 有值即用；PalmDB 名回落带零填充尾（Palm 约定 32B 补零），
/// 去尾零与首尾空白。
fn normalize_title(title: String) -> String {
    title.trim_end_matches('\0').trim().to_string()
}

/// 取 EXTH 某记录类型的**全部**取值（可重复记录），各自 lossy 解码去空，`; ` 连接。
/// mobi crate 的 `author()` 只取第一条（get_record_string_lossy → first），
/// 多作者书会丢后续——这里走底层 `exth_record` 拿全量（口径对齐 epub.authors）。
fn all_exth_strings(book: &Mobi, record: ExthRecord) -> String {
    book.metadata
        .exth_record(record)
        .map(|records| {
            records
                .iter()
                .map(|d| String::from_utf8_lossy(d).trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default()
}

/// 语言规范化：EXTH 524（字符串形态，如 `zh-cn`）优先——比 MOBI 头 locale
/// 字节精确；缺省回落 locale 字节枚举的 Debug 小写（document-parser 同款
/// 口径，vendor/.../formats/mobi/loader.rs L164-174）。Neutral/Unknown 记空串
/// （调用方不落 meta 键，与 epub.rs"取不到不插键"一致）。
fn language_of(book: &Mobi) -> String {
    if let Some(records) = book.metadata.exth_record_at(524) {
        if let Some(v) = records.first() {
            let s = String::from_utf8_lossy(v).trim().to_ascii_lowercase();
            if !s.is_empty() {
                return s;
            }
        }
    }
    match format!("{:?}", book.language()).to_ascii_lowercase().as_str() {
        "unknown" | "neutral" | "" => String::new(),
        s => s.to_string(),
    }
}

// ── 能力 / 插件 ─────────────────────────────────────────────────────

/// 原始读取：整篇正文 HTML 进 `RawPage.items[0]`，书籍元数据进 `RawDoc.meta`。
#[derive(Default)]
pub struct MobiRawBackend;

impl ParseCapability for MobiRawBackend {
    fn id(&self) -> &str {
        "mobi.read"
    }
    fn formats(&self) -> &[&str] {
        // azw3 走 kf8 自研支路（见模块头）；prc 是 MOBI/PalmDoc 的同构扩展名
        &["mobi", "prc", "azw3"]
    }
    fn parse(&self, path: &Path) -> Result<RawDoc> {
        // 整件读入一次：KF8 侦测与 mobi crate 通路共用同一份字节
        let bytes =
            std::fs::read(path).map_err(|e| DocError::corrupt(format!("MOBI 读取失败: {e}")))?;
        // KF8（azw3）优先：混合件按 EXTH 121 boundary 剥离取 KF8 部分
        // （翻译场景新格式优先，取舍见模块头与 kf8 模块头）。
        // Ok(None) = 非 KF8 → 回落 mobi crate 的 MOBI7 通路；
        // Err = encrypted/corrupt 终止码（已识别 KF8 不再回落）。
        if let Some(raw) = kf8::try_parse(&bytes)? {
            return Ok(raw);
        }
        let book = Mobi::from_read(std::io::Cursor::new(&bytes))
            .map_err(|e| DocError::corrupt(format!("MOBI 打开失败: {e}")))?;

        // 加密件是终止码不回退（docparse-plan §6）：PalmDoc 头 encryption 字段
        // （0=无/1=旧 Mobipocket/2=Mobipocket）+ MOBI 头 drm_offset 指到 DRM
        // 记录（has_drm，mobih.rs L390-392）任一命中即拒绝
        if book.encryption() != Encryption::No || book.metadata.mobi.has_drm() {
            return Err(DocError::encrypted("MOBI 带 DRM 加密，不支持解密"));
        }

        // 正文：readable 记录区间解压出的整篇 HTML。用 lossy 版本——单字节
        // 编码瑕疵不致命整个文件（翻译场景宁要占位符不要失败）
        let html = book.content_as_string_lossy();
        if html.trim().is_empty() {
            // Huff 解压失败时 mobi-rs 静默返回空串（lib.rs L325
            // unwrap_or_default）：把"静默空"升格为 corrupt 交给上层回退链
            return Err(DocError::corrupt(
                "MOBI 正文为空（不支持的压缩/内容形态，或纯图像书）",
            ));
        }

        let mut meta = HashMap::new();
        let title = normalize_title(book.title());
        if !title.is_empty() {
            meta.insert("mobi.title".to_string(), title);
        }
        let authors = all_exth_strings(&book, ExthRecord::Author);
        if !authors.is_empty() {
            meta.insert("mobi.authors".to_string(), authors);
        }
        let language = language_of(&book);
        if !language.is_empty() {
            meta.insert("mobi.language".to_string(), language);
        }

        Ok(RawDoc {
            pages: vec![RawPage {
                items: vec![RawTextItem {
                    text: html,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            meta,
            needs_ocr: false,
        })
    }
}

/// MOBI 插件：整篇 HTML → 结构化块（单页文档）。
///
/// 取舍（相对 epub.rs 的按文档内标题切节）：**不切节**。MOBI7 无 spine/NCX
/// 等章界结构，章界靠 `<mbp:pagebreak>` 与标题的松散约定（parse_blocks 不
/// 产出 pagebreak 位置信息），且真实语料里标题层级混乱（自 Word/网页迁移的
/// 书常全文 h1）；翻译场景需要的是稳定段落边界而不是猜测的章界——整篇块化
/// （plugins::html 的 HtmlPlugin 同款口径）+ 元数据透传 `meta.extra`。
pub struct MobiPlugin;

impl FormatPlugin for MobiPlugin {
    fn id(&self) -> &str {
        "mobi"
    }
    fn formats(&self) -> &[&str] {
        &["mobi", "prc", "azw3"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["mobi.read"]
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        // opt-in 链：能力未注册时 resolve 静默跳过（registry filter_map 语义）
        #[allow(unused_mut)]
        let mut chain: Vec<&'static str> = vec!["ocr-images"];
        #[cfg(feature = "lang-detect")]
        chain.push("lang-detect");
        chain
    }
    fn assemble(&self, raw: &RawDoc) -> Document {
        let html = raw
            .pages
            .first()
            .and_then(|p| p.items.first())
            .map(|i| i.text.as_str())
            .unwrap_or_default();
        let blocks = parse_blocks(html);
        // 页文本 = 非空 content 以空行连接（Image 块 content 为空不进文本，
        // 与 HtmlPlugin 同口径）
        let text = blocks
            .iter()
            .filter(|b| !b.content.is_empty())
            .map(|b| b.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut doc = Document {
            pages: vec![Page {
                blocks,
                text,
                ..Default::default()
            }],
            ..Default::default()
        };
        for key in ["mobi.title", "mobi.authors", "mobi.language"] {
            if let Some(v) = raw.meta.get(key) {
                doc.meta.extra.insert(key.to_string(), v.clone());
            }
        }
        doc
    }
}

/// 注册 MOBI 后端（解析能力 + 插件）。
pub fn register_defaults(reg: &mut Registry) {
    let _ = reg.register_parse(Arc::new(MobiRawBackend), None);
    reg.register_plugin(Arc::new(MobiPlugin));
}

// ── 测试：构造最小 MOBI7 金测（mobi crate 只读、无语料 → 手工编字节） ──

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BlockKind;
    use std::path::PathBuf;

    /// 测试件参数（构造最小 MOBI7）。
    struct Fixture {
        /// PalmDoc 头 encryption 字段：0=无 / 2=Mobipocket 加密
        encryption: u16,
        /// MOBI 头 drm_offset：false=0xFFFFFFFF（无 DRM）/ true=100（指到 DRM 记录）
        drm: bool,
        /// 是否挂 EXTH 头（书名/作者/语言字符串记录）
        exth: bool,
        /// PalmDB 名（title() 的 EXTH 缺省回落源）
        name: &'static str,
        /// 正文 HTML（单条无压缩 UTF-8 记录）
        content: &'static str,
    }

    fn be16(v: u16, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn be32(v: u32, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    /// 手工编最小 MOBI7：PDB 头(78B) + 记录表(2×8B + 2B gap) + 记录 0
    /// （PalmDoc 头 16B + MOBI 头 232B + EXTH? + 全名）+ 记录 1（正文）。
    /// 字节布局严格对齐 mobi-rs 解析路径
    /// （headers/{header,palmdoch,mobih,exth}.rs 与 record.rs）。
    fn build_mobi(dir_name: &str, f: &Fixture) -> PathBuf {
        let text = f.content.as_bytes().to_vec();

        // ── 记录 0 ──
        let mut rec0 = Vec::new();
        // PalmDoc 头（palmdoch.rs L100-110 读取序，16B）
        be16(1, &mut rec0); // compression = 无压缩
        be16(0, &mut rec0); // unused
        be32(text.len() as u32, &mut rec0); // text_length
        be16(1, &mut rec0); // record_count = 1（正文一条记录）
        be16(4096, &mut rec0); // record_size
        be16(f.encryption, &mut rec0); // encryption
        be16(0, &mut rec0); // unused

        // MOBI 头字段体（mobih.rs L242-327 读取序；identifier 之后 224B，
        // header_length 记 232 = 8 前缀 + 224 字段体，真实 kindlegen 口径）
        let mut body = Vec::new();
        be32(2, &mut body); // mobi_type = MobiPocketBook
        be32(65001, &mut body); // text_encoding = UTF-8
        be32(0, &mut body); // id
        be32(6, &mut body); // gen_version
        be32(!0, &mut body); // ortho_index
        be32(!0, &mut body); // inflect_index
        be32(!0, &mut body); // index_names
        be32(!0, &mut body); // index_keys
        for _ in 0..6 {
            be32(!0, &mut body); // extra_indices ×6
        }
        // first_non_book_index：按格式语义应填 2（正文止于记录 1 之后），但
        // mobi-rs 的 RawRecords::range 存在双重差一（record.rs L88：end =
        // (b-1).min(len-1)，任何 range 都覆盖不到末条记录）——2 条记录的书
        // range(1..3) 会坍缩为 [1..1] 空集。真实书籍记录数远大于区间边界、
        // 不受影响；构造件补 1 条尾部哨兵记录（num_records=3），使
        // range(1..3) = [1..2] 恰好覆盖唯一正文记录
        be32(3, &mut body);
        let name_off_at = body.len(); // name_offset 占位（记录 0 定长后回填）
        be32(0, &mut body);
        be32(f.name.len() as u32, &mut body); // name_length
        be16(0, &mut body); // unused
        body.push(9); // locale = 0x09（英文）
        body.push(9); // language_code = 9 → Language::English
        be32(0, &mut body); // input_language
        be32(0, &mut body); // output_language
        be32(6, &mut body); // format_version（MOBI7 = 6）
        be32(!0, &mut body); // first_image_index
        be32(0, &mut body); // first_huff_record
        be32(0, &mut body); // huff_record_count
        be32(0, &mut body); // huff_table_offset
        be32(0, &mut body); // huff_table_length
        be32(if f.exth { 0x40 } else { 0 }, &mut body); // exth_flags：0x40 = 有 EXTH
        body.extend_from_slice(&[0; 32]); // unused_0
        be32(!0, &mut body); // unused_1
        // drm_offset：非 0xFFFFFFFF 即视为有 DRM（has_drm）
        be32(if f.drm { 100 } else { !0 }, &mut body);
        be32(0, &mut body); // drm_count
        be32(0, &mut body); // drm_size
        be32(0, &mut body); // drm_flags
        body.extend_from_slice(&[0; 8]); // unused_2
        be16(1, &mut body); // first_content_record
        be16(1, &mut body); // last_content_record
        be32(0, &mut body); // unused_3
        be32(!0, &mut body); // fcis_record
        be32(!0, &mut body); // unused_4
        be32(!0, &mut body); // flis_record
        be32(!0, &mut body); // unused_5
        be32(0, &mut body); // unused_6（u64 低半）
        be32(0, &mut body); // unused_6（u64 高半）
        be32(!0, &mut body); // unused_7
        be32(0, &mut body); // first_compilation_data_section_count
        be32(!0, &mut body); // data_section_count
        be32(!0, &mut body); // unused_8
        be32(0, &mut body); // extra_record_data_flags
        be32(!0, &mut body); // first_index_record
        assert_eq!(body.len(), 224, "MOBI 头字段体应为 224B");

        rec0.extend_from_slice(b"MOBI");
        be32(232, &mut rec0); // header_length（8 前缀 + 224 字段体）
        rec0.extend_from_slice(&body);

        // EXTH（exth.rs L217-259）：12B 头 + N×(type u32 + len u32 + data)
        if f.exth {
            let mut exth = Vec::new();
            let mut count = 0u32;
            let mut put = |ty: u32, data: &[u8]| {
                be32(ty, &mut exth);
                be32(8 + data.len() as u32, &mut exth);
                exth.extend_from_slice(data);
                count += 1;
            };
            put(100, "作者甲; 作者乙".as_bytes()); // Author（可重复记录）
            put(503, "测试书".as_bytes()); // Title（title() 优先源）
            put(524, b"zh-cn"); // Language（字符串形态）
            let mut hdr = Vec::new();
            hdr.extend_from_slice(b"EXTH");
            be32(12 + exth.len() as u32, &mut hdr);
            be32(count, &mut hdr);
            rec0.extend_from_slice(&hdr);
            rec0.extend_from_slice(&exth);
        }

        // 4 字节对齐后挂 PalmDB 全名（name_offset 相对记录 0 起点，
        // headers/mod.rs L87-99 用 records[0].offset + name_offset 定位）
        while rec0.len() % 4 != 0 {
            rec0.push(0);
        }
        let name_offset = rec0.len() as u32;
        rec0.extend_from_slice(f.name.as_bytes());
        let name_off_pos = 16 + 8 + name_off_at; // PalmDoc 头 + "MOBI" + header_length 之后
        rec0[name_off_pos..name_off_pos + 4].copy_from_slice(&name_offset.to_be_bytes());

        // ── PDB 头 + 记录表（header.rs L44-81；gap 字段 = PdbRecords.extra_bytes，恒 0）──
        let mut out = Vec::new();
        let mut pdb_name = f.name.as_bytes().to_vec();
        pdb_name.resize(32, 0);
        out.extend_from_slice(&pdb_name); // 32B 名
        be16(0, &mut out); // attributes
        be16(0, &mut out); // version
        be32(0, &mut out); // created
        be32(0, &mut out); // modified
        be32(0, &mut out); // backup
        be32(0, &mut out); // modnum
        be32(0, &mut out); // app_info_id
        be32(0, &mut out); // sort_info_id
        out.extend_from_slice(b"BOOK"); // type
        out.extend_from_slice(b"MOBI"); // creator
        be32(2, &mut out); // unique_id_seed
        be32(0, &mut out); // next_record_list_id
        be16(3, &mut out); // num_records = 3（记录 0 + 正文 + 尾部哨兵）
        let rec0_off = out.len() as u32 + 3 * 8 + 2; // 78 + 24 + 2 = 104
        let rec1_off = rec0_off + rec0.len() as u32;
        let rec2_off = rec1_off + text.len() as u32; // 末条哨兵：offset = 文件尾，切片为空
        be32(rec0_off, &mut out);
        be32(0, &mut out);
        be32(rec1_off, &mut out);
        be32(1, &mut out);
        be32(rec2_off, &mut out);
        be32(2, &mut out);
        be16(0, &mut out); // gap（extra_bytes = 0）
        debug_assert_eq!(out.len() as u32, rec0_off);
        out.extend_from_slice(&rec0);
        out.extend_from_slice(&text);
        // 哨兵记录体为 0 字节（offset 指向文件尾即空切片）

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("mobi-{dir_name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("book.mobi");
        std::fs::write(&path, &out).unwrap();
        path
    }

    fn full_fixture() -> Fixture {
        Fixture {
            encryption: 0,
            drm: false,
            exth: true,
            name: "testbook",
            content: "<html><head><title>内页标题</title></head><body>\
                      <h1>第一章 标题</h1><p>正文一段。</p><p>正文二段。</p>\
                      <mbp:pagebreak/></body></html>",
        }
    }

    /// 金测（构造真件走真读取路径）：EXTH 书名/作者/语言 + 整篇 HTML 进 items[0]。
    #[test]
    fn reads_metadata_and_content_from_constructed_mobi7() {
        let p = build_mobi("full", &full_fixture());
        let raw = MobiRawBackend.parse(&p).unwrap();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());

        assert_eq!(raw.meta.get("mobi.title").map(String::as_str), Some("测试书"));
        assert_eq!(
            raw.meta.get("mobi.authors").map(String::as_str),
            Some("作者甲; 作者乙")
        );
        assert_eq!(raw.meta.get("mobi.language").map(String::as_str), Some("zh-cn"));
        assert_eq!(raw.pages.len(), 1);
        assert!(raw.pages[0].items[0].text.contains("正文一段。"));
        assert!(raw.pages[0].items[0].text.contains("<mbp:pagebreak/>"));
        assert!(!raw.needs_ocr);
    }

    /// HTML 块化路径：head/title 跳过，正文 = 标题 + 两段；元数据透传 meta.extra。
    #[test]
    fn assembles_html_into_structured_blocks() {
        let p = build_mobi("asm", &full_fixture());
        let raw = MobiRawBackend.parse(&p).unwrap();
        let doc = MobiPlugin.assemble(&raw);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());

        assert_eq!(doc.pages.len(), 1);
        let kinds: Vec<BlockKind> = doc.pages[0].blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![BlockKind::Heading, BlockKind::Paragraph, BlockKind::Paragraph]
        );
        assert_eq!(doc.pages[0].blocks[0].content, "第一章 标题");
        assert_eq!(doc.pages[0].text, "第一章 标题\n\n正文一段。\n\n正文二段。");
        assert_eq!(doc.meta.extra.get("mobi.title").map(String::as_str), Some("测试书"));
        assert_eq!(doc.meta.extra.get("mobi.language").map(String::as_str), Some("zh-cn"));
    }

    /// EXTH 缺省回落：书名取 PalmDB 名（去零填充尾），语言取 MOBI 头 locale
    /// 字节枚举的 Debug 小写；无作者记录则不落键。
    #[test]
    fn falls_back_to_palm_name_and_locale_language_without_exth() {
        let p = build_mobi(
            "minimal",
            &Fixture {
                exth: false,
                name: "testbook",
                ..full_fixture()
            },
        );
        let raw = MobiRawBackend.parse(&p).unwrap();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());

        assert_eq!(raw.meta.get("mobi.title").map(String::as_str), Some("testbook"));
        assert_eq!(raw.meta.get("mobi.language").map(String::as_str), Some("english"));
        assert!(!raw.meta.contains_key("mobi.authors"));
    }

    /// 加密件是终止码（encrypted），不进回退链：encryption 字段与 drm_offset
    /// 两条判定路径分别验证。
    #[test]
    fn encrypted_or_drm_mobi_is_encrypted_error() {
        let p = build_mobi("enc", &Fixture { encryption: 2, ..full_fixture() });
        let err = MobiRawBackend.parse(&p).unwrap_err();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        assert_eq!(err.code.as_str(), "encrypted");

        let p = build_mobi("drm", &Fixture { drm: true, ..full_fixture() });
        let err = MobiRawBackend.parse(&p).unwrap_err();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        assert_eq!(err.code.as_str(), "encrypted");
    }

    #[test]
    fn garbage_is_corrupt() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("mobi-garbage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.mobi");
        std::fs::write(
            &path,
            b"this file is definitely not a mobi document, not even close......",
        )
        .unwrap();
        let err = MobiRawBackend.parse(&path).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(err.code.as_str(), "corrupt");
    }

    // ── KF8/azw3 测试件（手工编字节；语义来源见 kf8 模块头）──

    /// 变长 7 位编码（INDX tag 值）：高位组在前、终止字节高位置 1
    /// （v=0 → [0x80]）。
    fn vwi(v: u64, out: &mut Vec<u8>) {
        let mut groups = Vec::new();
        let mut x = v;
        loop {
            groups.push((x & 0x7F) as u8);
            x >>= 7;
            if x == 0 {
                break;
            }
        }
        // 输出高位组在前（无高位），末字节（最低组）高位置 1 终止
        for i in (0..groups.len()).rev() {
            let mut b = groups[i];
            if i == 0 {
                b |= 0x80;
            }
            out.push(b);
        }
    }

    /// KF8 测试件参数。
    struct Kf8Fixture {
        /// PalmDoc 头 encryption 字段：0=无 / 2=加密
        encryption: u16,
        /// KF8 头 drm_offset：false=0xFFFFFFFF / true=100
        drm: bool,
        /// 骨架 HTML（尾部必须含 `</body></html>`，默认插入点在其前）
        skel: &'static str,
        /// 片段 HTML（各为一条记录）
        frags: &'static [&'static str],
        /// 各片段插入位置（相对骨架起点；None = `</body></html>` 之前）
        insert_positions: Option<Vec<usize>>,
        /// 正文记录压缩：false=无压缩 / true=PalmDoc（内容须为 ASCII）
        palmdoc: bool,
    }

    impl Kf8Fixture {
        fn positions(&self) -> Vec<usize> {
            self.insert_positions.clone().unwrap_or_else(|| {
                vec![self.skel.len() - "</body></html>".len(); self.frags.len()]
            })
        }
    }

    /// 默认 KF8 件：骨架 h1 + 单片段两段（UTF-8 中文，无压缩）。
    fn kf8_fixture() -> Kf8Fixture {
        Kf8Fixture {
            encryption: 0,
            drm: false,
            skel: "<html><head><title>内页标题</title></head><body><h1>第一章 标题</h1></body></html>",
            frags: &["<p>正文一段。</p><p>正文二段。</p>"],
            insert_positions: None,
            palmdoc: false,
        }
    }

    /// INDX 主记录：13×u32 头（len=56，count=1）+ TAGX 段（cbc=1）。
    fn indx_main(tags: &[(u8, u8, u8, u8)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"INDX");
        be32(56, &mut out); // len（TAGX 在 56）
        for v in [0u32, 0, 0, 0, 1, 65001, 0, 1, 0, 0, 0, 0] {
            be32(v, &mut out); // nul1,type,gen,start,count,code,lng,total,ordt,ligt,nligt,nctoc
        }
        out.extend_from_slice(b"TAGX");
        be32(12 + (tags.len() * 4) as u32, &mut out); // first_entry_offset
        be32(1, &mut out); // control_byte_count
        for t in tags {
            out.extend_from_slice(&[t.0, t.1, t.2, t.3]);
        }
        out
    }

    /// INDX 条目记录：条目 = [u8 label 长度][label][控制字节][tag 值...]，
    /// IDXT 段在头 start 字段偏移（魔数 + u16 条目起点表）。
    fn indx_entry_rec(entries: &[(String, u8, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"INDX");
        for _ in 0..13 {
            be32(0, &mut out);
        }
        let mut body = Vec::new();
        let mut starts = Vec::new();
        for (label, cb, values) in entries {
            starts.push(56 + body.len());
            body.push(label.len() as u8);
            body.extend_from_slice(label.as_bytes());
            body.push(*cb);
            body.extend_from_slice(values);
        }
        let idxt_pos = 56 + body.len();
        out[4..8].copy_from_slice(&56u32.to_be_bytes());
        out[20..24].copy_from_slice(&(idxt_pos as u32).to_be_bytes());
        out[24..28].copy_from_slice(&(entries.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(b"IDXT");
        for s in &starts {
            be16(*s as u16, &mut out);
        }
        out
    }

    /// FDST 记录：1 组 (0, rawML 长度) → flow 0 = 整个 rawML。
    fn fdst_record(raw_len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"FDST");
        be32(0, &mut out); // 保留位
        be32(1, &mut out); // 组数
        be32(0, &mut out); // flow0 起点
        be32(raw_len as u32, &mut out); // flow0 终点
        out
    }

    /// KF8 部分记录序列（相对记录号 0..）：0=KF8 头（header_length 0xF8，
    /// EXTH 503/100/524），1=骨架（带 multibyte trailing `41 01`，
    /// extra_flags=0x1），2..=片段，随后 SKEL INDX 主、SKEL 条目、
    /// FRAG INDX 主、FRAG 条目、FDST。
    fn build_kf8_records(f: &Kf8Fixture) -> Vec<Vec<u8>> {
        let skel_content = f.skel.as_bytes();
        let mut raw_ml: Vec<u8> = skel_content.to_vec();
        let mut frag_spans = Vec::new(); // 片段在 rawML 中的 (起点, 长度)
        for fr in f.frags {
            frag_spans.push((raw_ml.len(), fr.len()));
            raw_ml.extend_from_slice(fr.as_bytes());
        }
        let n_text = (1 + f.frags.len()) as u32; // 骨架 + 片段记录数

        // ── 记录 0：PalmDoc 头 + MOBI 头（header_length 248）+ EXTH + 全名 ──
        let mut rec0 = Vec::new();
        be16(if f.palmdoc { 2 } else { 1 }, &mut rec0); // compression
        be16(0, &mut rec0); // unused
        be32(raw_ml.len() as u32, &mut rec0); // text_length
        be16(n_text as u16, &mut rec0); // record_count = rawML 记录数
        be16(4096, &mut rec0); // record_size
        be16(f.encryption, &mut rec0); // encryption
        be16(0, &mut rec0); // unused
        rec0.extend_from_slice(b"MOBI");
        be32(248, &mut rec0); // header_length（KF8 = 0xF8；EXTH 在 16+248=264）
        let mut body = vec![0u8; 240]; // 字段体 [24, 264)
        body[0..4].copy_from_slice(&2u32.to_be_bytes()); // mobi_type @24
        body[4..8].copy_from_slice(&65001u32.to_be_bytes()); // text_encoding = UTF-8 @28
        body[12..16].copy_from_slice(&6u32.to_be_bytes()); // gen_version @36
        body[56..60].copy_from_slice(&(n_text + 1).to_be_bytes()); // first_non_book @80
        body[80..84].copy_from_slice(&8u32.to_be_bytes()); // format_version = 8 @104
        body[104..108].copy_from_slice(&0x40u32.to_be_bytes()); // exth_flags @128
        body[144..148].copy_from_slice(&(if f.drm { 100 } else { !0u32 }).to_be_bytes()); // drm_offset @168
        body[168..172].copy_from_slice(&(n_text + 5).to_be_bytes()); // fdst 记录号 @192（相对）
        body[172..176].copy_from_slice(&2u32.to_be_bytes()); // fdst_cnt @196（>1 才采信）
        body[216..220].copy_from_slice(&1u32.to_be_bytes()); // extra_flags = 0x1（multibyte）@240
        body[220..224].copy_from_slice(&(!0u32).to_be_bytes()); // first_index @244
        body[224..228].copy_from_slice(&(n_text + 3).to_be_bytes()); // frag_idx @248（相对）
        body[228..232].copy_from_slice(&(n_text + 1).to_be_bytes()); // skel_idx @252（相对）
        body[232..236].copy_from_slice(&(!0u32).to_be_bytes()); // guide @256
        rec0.extend_from_slice(&body);

        // EXTH（KF8 头自带元数据：作者/书名/语言）
        let mut exth = Vec::new();
        let mut count = 0u32;
        let mut put = |ty: u32, data: &[u8]| {
            be32(ty, &mut exth);
            be32(8 + data.len() as u32, &mut exth);
            exth.extend_from_slice(data);
            count += 1;
        };
        put(100, "作者甲; 作者乙".as_bytes()); // Author
        put(503, "KF8测试书".as_bytes()); // Updated Title
        put(524, b"zh-cn"); // Language
        let mut hdr = Vec::new();
        hdr.extend_from_slice(b"EXTH");
        be32(12 + exth.len() as u32, &mut hdr);
        be32(count, &mut hdr);
        rec0.extend_from_slice(&hdr);
        rec0.extend_from_slice(&exth);
        // 4 字节对齐后挂全名（name_offset 相对记录 0 起点 @84，长度 @88）
        while rec0.len() % 4 != 0 {
            rec0.push(0);
        }
        let name_offset = rec0.len() as u32;
        rec0.extend_from_slice(b"kf8book");
        rec0[84..88].copy_from_slice(&name_offset.to_be_bytes());
        rec0[88..92].copy_from_slice(&7u32.to_be_bytes());

        // ── 正文记录：骨架（multibyte trailing）+ 片段 ──
        let mut recs = vec![rec0];
        let mut skel_rec = skel_content.to_vec();
        skel_rec.extend_from_slice(b"A\x01"); // multibyte trailing：末字节 0x01 → 剥 2 字节
        recs.push(skel_rec);
        // 片段记录同样带 multibyte trailing（extra_flags=0x1 对所有正文
        // 记录生效，解析端逐记录剥离）
        for fr in f.frags {
            let mut rec = fr.as_bytes().to_vec();
            rec.extend_from_slice(b"A\x01");
            recs.push(rec);
        }

        // ── SKEL INDX：tag1 = 片段个数，tag6 = [起点 0, 骨架长度] ──
        // TAGX：数据 tag endFlag=0；末位 tag0 = 控制字节边界标记（endFlag=1）
        recs.push(indx_main(&[(1, 1, 0x02, 0x00), (6, 2, 0x04, 0x00), (0, 0, 0x08, 0x01)]));
        let mut skel_vals = Vec::new();
        vwi(1, &mut skel_vals); // fragment 个数
        vwi(0, &mut skel_vals); // 骨架起点
        vwi(skel_content.len() as u64, &mut skel_vals); // 骨架长度
        recs.push(indx_entry_rec(&[("part00000".to_string(), 0x06, skel_vals)]));

        // ── FRAG INDX：label = 十进制插入位置，tag3/4 = 文件号/序号，
        //    tag6 = [rawML 起点, 长度]，tag2 = ctoc 偏移（解析端不消费）──
        recs.push(indx_main(&[
            (3, 1, 0x02, 0x00),
            (4, 1, 0x04, 0x00),
            (6, 2, 0x08, 0x00),
            (2, 1, 0x10, 0x00),
            (0, 0, 0x20, 0x01),
        ]));
        let positions = f.positions();
        let frag_entries: Vec<(String, u8, Vec<u8>)> = f
            .frags
            .iter()
            .enumerate()
            .map(|(j, _)| {
                let mut v = Vec::new();
                vwi(0, &mut v); // 文件号
                vwi(j as u64, &mut v); // 序号
                vwi(frag_spans[j].0 as u64, &mut v); // rawML 起点
                vwi(frag_spans[j].1 as u64, &mut v); // 长度
                vwi(0, &mut v); // ctoc 偏移
                (positions[j].to_string(), 0x1E, v)
            })
            .collect();
        recs.push(indx_entry_rec(&frag_entries));

        // ── FDST ──
        recs.push(fdst_record(raw_ml.len()));
        recs
    }

    /// PDB 容器装配：78B 头 + 记录表 + 2B gap + 记录体。
    fn assemble_pdb(pdb_name: &str, recs: Vec<Vec<u8>>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut name = pdb_name.as_bytes().to_vec();
        name.resize(32, 0);
        out.extend_from_slice(&name);
        be16(0, &mut out); // attributes
        be16(0, &mut out); // version
        be32(0, &mut out); // created
        be32(0, &mut out); // modified
        be32(0, &mut out); // backup
        be32(0, &mut out); // modnum
        be32(0, &mut out); // app_info_id
        be32(0, &mut out); // sort_info_id
        out.extend_from_slice(b"BOOK"); // type
        out.extend_from_slice(b"MOBI"); // creator
        be32(2, &mut out); // unique_id_seed
        be32(0, &mut out); // next_record_list_id
        be16(recs.len() as u16, &mut out); // num_records
        let rec0_off = (out.len() + recs.len() * 8 + 2) as u32;
        let mut off = rec0_off;
        for (i, r) in recs.iter().enumerate() {
            be32(off, &mut out);
            be32(i as u32, &mut out);
            off += r.len() as u32;
        }
        be16(0, &mut out); // gap
        debug_assert_eq!(out.len() as u32, rec0_off);
        for r in &recs {
            out.extend_from_slice(r);
        }
        out
    }

    fn write_fixture(dir_name: &str, file_name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("mobi-{dir_name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// 纯 KF8 件（.azw3，无 EXTH 121，format_version=8 → 整文件按 KF8 解析）。
    fn build_azw3(dir_name: &str, f: &Kf8Fixture) -> PathBuf {
        write_fixture(dir_name, "book.azw3", &assemble_pdb("kf8book", build_kf8_records(f)))
    }

    /// 混合件（.mobi）：MOBI7 头（EXTH 121 → KF8 起始记录号）+ 1 条
    /// MOBI7 正文 + KF8 部分。断言 KF8 部分胜出。
    fn build_hybrid_mobi(dir_name: &str) -> PathBuf {
        let kf8_recs = build_kf8_records(&kf8_fixture());
        let mobi7_html = b"<html><body><p>MOBI7 OLD TEXT</p></body></html>";
        let boundary = 2u32; // KF8 头从记录 2 开始（rec0 + MOBI7 正文之后）

        let mut rec0 = Vec::new();
        be16(1, &mut rec0); // compression = 无压缩
        be16(0, &mut rec0);
        be32(mobi7_html.len() as u32, &mut rec0);
        be16(1, &mut rec0); // record_count = 1（MOBI7 正文一条）
        be16(4096, &mut rec0);
        be16(0, &mut rec0); // encryption
        be16(0, &mut rec0);
        rec0.extend_from_slice(b"MOBI");
        be32(232, &mut rec0); // header_length（MOBI7 口径）
        let mut body = vec![0u8; 224];
        body[0..4].copy_from_slice(&2u32.to_be_bytes()); // mobi_type @24
        body[4..8].copy_from_slice(&65001u32.to_be_bytes()); // text_encoding @28
        body[12..16].copy_from_slice(&6u32.to_be_bytes()); // gen_version @36
        body[56..60].copy_from_slice(&2u32.to_be_bytes()); // first_non_book @80
        body[80..84].copy_from_slice(&6u32.to_be_bytes()); // format_version = 6 @104
        body[104..108].copy_from_slice(&0x40u32.to_be_bytes()); // exth_flags @128
        body[144..148].copy_from_slice(&(!0u32).to_be_bytes()); // drm_offset @168
        body[216..220].copy_from_slice(&0u32.to_be_bytes()); // extra_flags @240
        body[220..224].copy_from_slice(&(!0u32).to_be_bytes()); // first_index @244
        rec0.extend_from_slice(&body);
        // EXTH：503 书名 + 121 boundary（KF8 起始记录号）
        let mut exth = Vec::new();
        be32(503, &mut exth);
        be32(8 + "混合书".len() as u32, &mut exth);
        exth.extend_from_slice("混合书".as_bytes());
        be32(121, &mut exth);
        be32(12, &mut exth);
        exth.extend_from_slice(&boundary.to_be_bytes());
        let mut hdr = Vec::new();
        hdr.extend_from_slice(b"EXTH");
        be32(12 + exth.len() as u32, &mut hdr);
        be32(2, &mut hdr);
        rec0.extend_from_slice(&hdr);
        rec0.extend_from_slice(&exth);
        while rec0.len() % 4 != 0 {
            rec0.push(0);
        }

        let mut recs = vec![rec0, mobi7_html.to_vec()];
        recs.extend(kf8_recs);
        write_fixture(dir_name, "hybrid.mobi", &assemble_pdb("hybrid", recs))
    }

    /// KF8 金测（构造真件走真读取路径）：骨架+片段装配出整篇 HTML，
    /// 元数据取 KF8 头 EXTH（503/100/524）。
    #[test]
    fn reads_azw3_kf8_content_and_metadata() {
        let p = build_azw3("kf8-full", &kf8_fixture());
        let raw = MobiRawBackend.parse(&p).unwrap();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());

        assert_eq!(raw.meta.get("mobi.title").map(String::as_str), Some("KF8测试书"));
        assert_eq!(
            raw.meta.get("mobi.authors").map(String::as_str),
            Some("作者甲; 作者乙")
        );
        assert_eq!(raw.meta.get("mobi.language").map(String::as_str), Some("zh-cn"));
        assert_eq!(raw.pages.len(), 1);
        let html = &raw.pages[0].items[0].text;
        assert!(html.contains("正文一段。"), "{html}");
        assert!(html.contains("</body></html>"));
        assert!(html.starts_with("<html>"));
        assert!(!raw.needs_ocr);
    }

    /// azw3 HTML 块化路径：骨架 h1 + 片段两段；元数据透传 meta.extra。
    #[test]
    fn assembles_azw3_into_structured_blocks() {
        let p = build_azw3("kf8-asm", &kf8_fixture());
        let raw = MobiRawBackend.parse(&p).unwrap();
        let doc = MobiPlugin.assemble(&raw);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        assert_eq!(doc.pages.len(), 1);
        let kinds: Vec<BlockKind> = doc.pages[0].blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![BlockKind::Heading, BlockKind::Paragraph, BlockKind::Paragraph]
        );
        assert_eq!(doc.pages[0].blocks[0].content, "第一章 标题");
        assert_eq!(doc.pages[0].text, "第一章 标题\n\n正文一段。\n\n正文二段。");
        assert_eq!(doc.meta.extra.get("mobi.title").map(String::as_str), Some("KF8测试书"));
        assert_eq!(doc.meta.extra.get("mobi.language").map(String::as_str), Some("zh-cn"));
    }

    /// 混合件（MOBI7+KF8 同包）：按 EXTH 121 剥离取 KF8 部分
    /// （翻译场景新格式优先），MOBI7 旧正文不出现。
    #[test]
    fn hybrid_mobi_prefers_kf8_part() {
        let p = build_hybrid_mobi("kf8-hybrid");
        let raw = MobiRawBackend.parse(&p).unwrap();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        let html = &raw.pages[0].items[0].text;
        assert!(html.contains("正文一段。"), "{html}");
        assert!(!html.contains("MOBI7 OLD TEXT"));
        assert_eq!(raw.meta.get("mobi.title").map(String::as_str), Some("KF8测试书"));
    }

    /// PalmDoc 压缩正文（ASCII 内容走字面量路径）正确解压 + trailing 剥离。
    #[test]
    fn reads_palmdoc_compressed_azw3() {
        let f = Kf8Fixture {
            skel: "<html><body><h1>Chapter One</h1></body></html>",
            frags: &["<p>Para one.</p>"],
            palmdoc: true,
            ..kf8_fixture()
        };
        let p = build_azw3("kf8-palmdoc", &f);
        let raw = MobiRawBackend.parse(&p).unwrap();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        let html = &raw.pages[0].items[0].text;
        assert!(html.contains("<p>Para one.</p>"), "{html}");
        assert!(!html.contains('\u{0}'), "无残留 trailing 字节");
    }

    /// KF8 加密件是终止码（encrypted），不回落 MOBI7：encryption 字段与
    /// drm_offset 两条判定路径分别验证。
    #[test]
    fn encrypted_kf8_is_encrypted_error() {
        let p = build_azw3("kf8-enc", &Kf8Fixture { encryption: 2, ..kf8_fixture() });
        let err = MobiRawBackend.parse(&p).unwrap_err();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        assert_eq!(err.code.as_str(), "encrypted");

        let p = build_azw3("kf8-drm", &Kf8Fixture { drm: true, ..kf8_fixture() });
        let err = MobiRawBackend.parse(&p).unwrap_err();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        assert_eq!(err.code.as_str(), "encrypted");
    }

    /// 垃圾 .azw3：PDB 不可读 → kf8 判非 KF8 → 回落 mobi crate → corrupt。
    #[test]
    fn garbage_azw3_is_corrupt() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("mobi-garbage-azw3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.azw3");
        std::fs::write(&path, b"definitely not an azw3 document at all........").unwrap();
        let err = MobiRawBackend.parse(&path).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(err.code.as_str(), "corrupt");
    }

    /// 真实语料金测位：mobi-0.8.0 存档无自带样例（无 tests/ 目录，核实于
    /// registry 缓存）。人工放入 tests/fixtures/mobi/（.mobi/.prc/.azw3，
    /// 注明来源与许可）后运行 `cargo test --features mobi -- --ignored`。
    /// 逐件断言：打开成功 + 书名非空 + 正文有非空块。
    #[test]
    #[ignore = "缺 .mobi/.prc/.azw3 真实语料测试件（放入 tests/fixtures/mobi/ 后运行 --ignored）"]
    fn real_corpus_golden() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mobi");
        let entries = std::fs::read_dir(&dir).expect("tests/fixtures/mobi/ 目录应存在");
        let mut checked = 0;
        for e in entries.flatten() {
            let path = e.path();
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if !matches!(ext.to_ascii_lowercase().as_str(), "mobi" | "prc" | "azw3") {
                continue;
            }
            let raw = MobiRawBackend
                .parse(&path)
                .unwrap_or_else(|e| panic!("{} 打开失败: {e}", path.display()));
            assert!(
                raw.meta.contains_key("mobi.title"),
                "{} 应解析出书名",
                path.display()
            );
            let doc = MobiPlugin.assemble(&raw);
            assert!(
                !doc.pages.is_empty()
                    && doc.pages[0].blocks.iter().any(|b| !b.content.is_empty()),
                "{} 正文应有非空块",
                path.display()
            );
            checked += 1;
        }
        assert!(checked > 0, "tests/fixtures/mobi/ 内没有 .mobi/.prc 样例");
    }
}
