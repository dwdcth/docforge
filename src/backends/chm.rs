//! CHM 后端（Microsoft Compiled HTML Help，feature = "chm"，中文技术书存量）。
//!
//! 支持面（libchm 0.3.1 依源码核实，registry 缓存
//! `~/.cargo/registry/src/mirrors.ustc.edu.cn-38d0e5eb5da2abae/libchm-0.3.1/`）：
//! * 纯 Rust reader（`unsafe_code = "forbid"`，MIT；依赖 bitflags + thiserror，
//!   Cargo.toml L24-44），ITSF/ITSP/PMGL 目录解析 + 内置 LZX 解压。API 形态：
//!   `ChmFile::open`（src/chm.rs L58）→ `entries(EntrySel)`（L162，
//!   NORMAL/SPECIAL/META × FILES/DIRS 位选）→ `find`（L113，路径大小写
//!   不敏感）→ `read/read_path`（L122/L152：未压缩段直读、MSCompressed 段
//!   LZX 解压）。条目路径带前导 `/`，目录条目以 `/` 结尾（lib.rs L21-24）。
//! * **无 .hhc 目录树 API、无元数据 API、无 writer**（公开面仅
//!   `ChmFile/Entry/EntrySel/EntryKind/EntryCategory/ChmError` 六件，见
//!   lib.rs L50-59）——.hhc 解析、/#SYSTEM 元数据、测试件构造都在本模块自补。
//!
//! 阅读序（CHM 的 spine 等价物）：
//! 1. `/#SYSTEM` code 0（HHP 的 Contents file 名，NT 串；chmspec 5.3.3
//!    Table 5.2，nongnu.org/chmspec）指名的 .hhc；
//! 2. 兜底枚举条目里的 .hhc（取路径段最少者——根级 TOC 优先）。
//!
//! .hhc 是 HTML 形态（标签大写、`<param>`/`<LI>` 不闭合、非 XML 实体，
//! quick-xml 会拒），故按 `text/sitemap` 的 `<object>` **文档序**手写扫描
//! Name（标题）/Local（路径）对——顺序即章节序，与 plugins::html 的
//! 手写标签扫描同一家法。HHW 用编译机 ANSI 代码页写 .hhc（中文书 GBK
//! 常见），解码走 [`crate::encoding::decode`] 统一链。Local 去锚点/反斜杠
//! 归一/%XX 解码后按条目路径读取（libchm find 大小写不敏感，正好兜住
//! TOC 与目录的大小写出入）；读不到的条目跳过、全部读不到退到文件名
//! 兜底；同一路径多次出现（TOC 常把一个文件的不同锚点列成多项）按首次
//! 出现去重。无 .hhc 时兜底：全部 .htm/.html 条目按路径**大小写不敏感
//! 字典序**（CHM 目录自身的排序键）——取舍：CHM 无章节元数据可依，数字
//! 序文件（1.htm/10.htm/2.htm）会乱序，有 .hhc 的正常书不受影响。
//!
//! 元数据（`/#SYSTEM` TLV：DWORD 版本 + `[code u16 | data-len u16 | data]`，
//! 串为 NT（NUL 终止）；chmspec 5.3.3）：
//! * code 3 Title → `chm.title`；code 2 Default topic → `chm.default_topic`；
//!   code 4 首个 DWORD LCID → `chm.lcid`（十进制）；
//!   code 0 Contents file → 阅读序定位（内部用，不落 extra）。
//! * 串解码启发式 [`system_string`]：偶长且 UTF-16LE 特征（结尾 NUL 对或
//!   高位字节全零）→ UTF-16LE；否则统一解码链（chardetng 概率探测 +
//!   GB18030/Big5 固定回退——中文 ANSI 代码页书的主力路径）。
//!   libchm 无 chmlib 式 #SYSTEM 解封，此 TLV 解析为本模块实现；拿不到
//!   #SYSTEM 时各键不落（与 epub"取不到不插键"一致）。
//!
//! 数据流（epub/mobi 同款"原文整体交插件"模式）：
//! `ChmRawBackend`（ParseCapability）→ 每章 HTML 进一章 `items[0]`（.hhc
//! 标题 → `RawPage.name`）→ `ChmPlugin`（FormatPlugin）`assemble` 用
//! [`crate::plugins::html::parse_blocks`] 块化。图片条目（扩展名白名单——
//! CHM 无 media-type 清单）opt-in 提取进首页 `images`（epub 五件套模式；
//! `ocr-images` 处理能力识别后回填 `ocr_text`）。
//!
//! 测试件：libchm 只读、无 writer；未压缩段（space=0）条目不经 LZX 直读
//! （chm.rs L127-137，`load_decompressor` 对无 MSCompressed 段的件返回
//! `Ok(None)` L77-106），故按 format.rs 的 ITSF/ITSP/PMGL 二进制布局手工
//! 构造最小 CHM 金测（mobi 后端同款策略），压缩路径另以真实件覆盖
//! （tests/fixtures/chm/ 的 SVN Book CHM，来源与许可见 real_corpus_golden）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use libchm::{ChmFile, EntrySel};

use crate::error::{DocError, Result};
use crate::model::{Document, Page};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawImage, RawPage, RawTextItem};
use crate::plugins::html::parse_blocks;
use crate::registry::Registry;

/// CHM 内部元数据条目路径（Special 命名空间，libchm find 大小写不敏感）。
const SYSTEM_PATH: &str = "/#SYSTEM";

/// 图片条目扩展名白名单（CHM 无 media-type，按扩展名过滤；口径同 cbz.rs）。
const IMAGE_EXTS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "gif", "bmp"];

/// 章节条目扩展名（.hhc 兜底序的收集面；`.html`/`.htm` 双形态）。
const HTML_EXTS: [&str; 2] = ["htm", "html"];

fn ext_of(path: &str) -> String {
    path.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_html(path: &str) -> bool {
    HTML_EXTS.contains(&ext_of(path).as_str())
}

fn is_image(path: &str) -> bool {
    IMAGE_EXTS.contains(&ext_of(path).as_str())
}

/// 大小写不敏感字节序比较（CHM 目录键的排序口径，libchm directory.rs L144 同款）。
fn cmp_ignore_ascii_case(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

// ── .hhc 目录树解析（CHM 的 spine 等价物） ──────────────────────────

/// .hhc 的一个目录项：Name = 章节标题，Local = 目标条目路径（已归一）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct TocEntry {
    title: String,
    path: String,
}

/// 从 .hhc（HTML 形态）按文档序提取 text/sitemap 的 Name/Local 对。
///
/// 容错口径（真实 .hhc 由 HHW 生成，非严格 XML）：标签大小写随意、
/// `<param>`/`<LI>` 不闭合、`</object>` 缺失时到下一个 `<object` 或串尾。
/// 过滤：`text/site properties` 块与纯分组节点（有 Name 无 Local）自然出局
/// ——本函数只收 title 与 path 双全的项。
fn parse_hhc(html: &str) -> Vec<TocEntry> {
    // to_ascii_lowercase 只动 ASCII 单字节，UTF-8 多字节序列逐字节保留：
    // lower 与 html 字节偏移一一对应，可拿 lower 定位、回 html 切原文
    // （属性值大小写因此不丢失）。
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find("<object") {
        let open = pos + rel;
        let Some(tag_rel) = lower[open..].find('>') else {
            break;
        };
        let inner_start = open + tag_rel + 1;
        let inner_end = match lower[inner_start..].find("</object") {
            Some(r) => inner_start + r,
            // 残缺容错：无 </object> 时到下一个 <object 或串尾
            None => match lower[inner_start..].find("<object") {
                Some(r) => inner_start + r,
                None => lower.len(),
            },
        };
        let inner_raw = &html[inner_start..inner_end];
        let inner_lower = &lower[inner_start..inner_end];

        let mut name: Option<String> = None;
        let mut local: Option<String> = None;
        let mut p = 0usize;
        while let Some(r) = inner_lower[p..].find("<param") {
            let ptag_start = p + r;
            let Some(gt) = inner_lower[ptag_start..].find('>') else {
                break;
            };
            let ptag = &inner_raw[ptag_start..ptag_start + gt + 1];
            let attrs = tag_attrs(ptag);
            let get = |k: &str| {
                attrs
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| v.clone())
            };
            let key = get("name").map(|v| v.trim().to_ascii_lowercase());
            match key.as_deref() {
                Some("name") => name = get("value"),
                Some("local") => local = get("value"),
                _ => {}
            }
            p = ptag_start + gt + 1;
        }

        let title = name
            .map(|s| unescape_entities(&s))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let path = local
            .map(|s| normalize_local(&unescape_entities(&s)))
            .unwrap_or_default();
        if !title.is_empty() && !path.is_empty() {
            out.push(TocEntry { title, path });
        }
        pos = inner_end;
    }
    out
}

/// 单标签属性表：`name="v"` / `name='v'` / `name=v`（未引号值到空白或 `>` 止）。
/// 属性名归一小写、属性值保留原文；无值属性记空串。
fn tag_attrs(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = tag.as_bytes();
    let mut i = 0usize;
    // 跳过标签名
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
        i += 1;
    }
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b'>' {
            break;
        }
        // 属性名：到 `=` / 空白 / `>` 止
        let name_start = i;
        while i < bytes.len()
            && bytes[i] != b'='
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'>'
        {
            i += 1;
        }
        let name = tag[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            if !name.is_empty() {
                out.push((name, String::new()));
            }
            continue;
        }
        i += 1; // `=`
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let val;
        if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
            let q = bytes[i];
            i += 1;
            let vs = i;
            while i < bytes.len() && bytes[i] != q {
                i += 1;
            }
            val = tag[vs..i].to_string();
            if i < bytes.len() {
                i += 1; // 闭引号
            }
        } else {
            let vs = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                i += 1;
            }
            val = tag[vs..i].to_string();
        }
        out.push((name, val));
    }
    out
}

/// 实体解码（.hhc 标题里的 `&amp;` 等）：命名实体小集 + 数字/十六进制；
/// 未知实体原样保留（口径对齐 plugins::html 的 decode_entities）。
fn unescape_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        let end = tail.find(';').filter(|e| *e <= 10);
        let decoded = end.and_then(|e| {
            let body = &tail[..e];
            match body {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ if body.starts_with('#') => {
                    let digits = body[1..]
                        .strip_prefix(['x', 'X'])
                        .map(|h| u32::from_str_radix(h, 16).ok())
                        .unwrap_or_else(|| body[1..].parse::<u32>().ok());
                    digits.and_then(char::from_u32)
                }
                _ => None,
            }
        });
        match (decoded, end) {
            (Some(c), Some(e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            _ => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// hhc Local 值 → CHM 条目路径：反斜杠归一 / 去锚点 / %XX 解码 / 补前导 `/`。
/// CHM 条目路径以 `/` 开头、`/` 分隔（libchm 约定）；空串表示无法定位。
fn normalize_local(local: &str) -> String {
    let s = local.trim().replace('\\', "/");
    let s = s.split('#').next().unwrap_or("");
    let s = percent_decode(s);
    let s = s.trim_start_matches("./");
    if s.is_empty() {
        String::new()
    } else if s.starts_with('/') {
        s.to_string()
    } else {
        format!("/{s}")
    }
}

/// Local 值的 %XX 解码（`%20` 空格等；非法序列原样保留）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── /#SYSTEM 元数据（TLV；chmspec 5.3.3） ───────────────────────────

/// `/#SYSTEM` 里能拿到的书级元数据（取不到的字段为空串 / None）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SystemMeta {
    /// code 3：HHP [OPTIONS] Title（书名，NT 串）。
    title: String,
    /// code 2：HHP [OPTIONS] Default topic（默认页条目路径，NT 串）。
    default_topic: String,
    /// code 0：HHP [OPTIONS] Contents file（.hhc 名，NT 串）——阅读序定位用。
    contents_file: String,
    /// code 4 载荷首个 DWORD：HHP Language（LCID）。
    lcid: Option<u32>,
}

/// 解析 `/#SYSTEM` 字节：DWORD 版本 + `[code u16 | data-len u16 | data]` 序列。
/// 残缺尾条目（长度越界）就地停表；未知 code 跳过。
fn parse_system(bytes: &[u8]) -> SystemMeta {
    let mut meta = SystemMeta::default();
    let mut lcid: Option<u32> = None;
    if bytes.len() < 4 {
        return meta;
    }
    let mut pos = 4usize; // 跳过版本 DWORD（2=compat 1.0 / 3=compat 1.1+）
    while pos + 4 <= bytes.len() {
        let code = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
        let len = u16::from_le_bytes(bytes[pos + 2..pos + 4].try_into().unwrap()) as usize;
        let data_start = pos + 4;
        if data_start + len > bytes.len() {
            break;
        }
        let data = &bytes[data_start..data_start + len];
        match code {
            0 => meta.contents_file = system_string(data),
            2 => meta.default_topic = system_string(data),
            3 => meta.title = system_string(data),
            4 if data.len() >= 4 => {
                lcid = Some(u32::from_le_bytes(data[..4].try_into().unwrap()))
            }
            _ => {}
        }
        pos = data_start + len;
    }
    meta.lcid = lcid;
    meta
}

/// #SYSTEM 字符串解码（NT 串，编码依编译机代码页——无显式声明可依）：
/// 偶长且 UTF-16LE 特征（结尾 NUL 对，或高位字节全零的无终止 ASCII 串）→
/// UTF-16LE（解出控制字符则视为误判退回 ANSI 链）；否则统一解码链
/// （BOM/UTF-16/chardetng + GB18030/Big5 固定回退，中文 ANSI 书的主力路径；
/// 未开 `encoding` feature 时退化为 lossy-utf8）。
fn system_string(raw: &[u8]) -> String {
    let looks_utf16 = raw.len() >= 2
        && raw.len() % 2 == 0
        && ((raw[raw.len() - 2] == 0 && raw[raw.len() - 1] == 0)
            || raw.chunks_exact(2).all(|p| p[1] == 0x00));
    if looks_utf16 {
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        let s = String::from_utf16_lossy(&units)
            .trim_start_matches('\u{feff}')
            .trim_end_matches('\0')
            .trim()
            .to_string();
        if !s.chars().any(char::is_control) {
            return s;
        }
    }
    crate::encoding::decode(raw.to_vec())
        .trim_end_matches('\0')
        .trim()
        .to_string()
}

// ── 能力 / 插件 ─────────────────────────────────────────────────────

/// 原始读取：.hhc 序的每章 HTML 进一章 `items[0]`，书籍元数据进 `RawDoc.meta`。
#[derive(Default)]
pub struct ChmRawBackend {
    /// 提取图片条目字节进首页 `images`（opt-in，对齐 EPUB 的五件套；
    /// `ocr-images` 处理能力识别后回填 `ocr_text`）。
    pub ocr_images: bool,
}

impl ParseCapability for ChmRawBackend {
    fn id(&self) -> &str {
        "chm.read"
    }
    fn formats(&self) -> &[&str] {
        &["chm"]
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let mut chm = ChmFile::open(path)
            .map_err(|e| DocError::corrupt(format!("CHM 打开失败: {e}")))?;

        // ── 元数据：/#SYSTEM（拿不到不落键）──
        let system = chm.read_path(SYSTEM_PATH).ok().map(|b| parse_system(&b));
        let mut meta: HashMap<String, String> = HashMap::new();
        if let Some(sys) = &system {
            if !sys.title.is_empty() {
                meta.insert("chm.title".to_string(), sys.title.clone());
            }
            if !sys.default_topic.is_empty() {
                meta.insert("chm.default_topic".to_string(), sys.default_topic.clone());
            }
            if let Some(lcid) = sys.lcid {
                meta.insert("chm.lcid".to_string(), lcid.to_string());
            }
        }

        // ── 阅读序：.hhc（#SYSTEM code 0 指名 → 枚举兜底）──
        let toc_path = system
            .as_ref()
            .map(|s| normalize_local(&s.contents_file))
            .filter(|p| !p.is_empty() && p.to_ascii_lowercase().ends_with(".hhc"))
            .filter(|p| chm.find(p).is_ok())
            .or_else(|| pick_hhc(&mut chm));

        let mut pages: Vec<RawPage> = Vec::new();
        if let Some(hp) = &toc_path {
            if let Some(bytes) = chm.read_path(hp).ok() {
                let hhc = crate::encoding::decode(bytes);
                // 同一文件的不同锚点在 TOC 里常列成多项：按路径去重（首次为准）
                let mut seen: HashSet<String> = HashSet::new();
                for te in parse_hhc(&hhc) {
                    if !seen.insert(te.path.clone()) {
                        continue;
                    }
                    let Ok(bytes) = chm.read_path(&te.path) else {
                        continue; // TOC 指到缺条目：跳过不致命
                    };
                    let html = crate::encoding::decode(bytes);
                    pages.push(RawPage {
                        name: Some(te.title),
                        items: vec![RawTextItem {
                            text: html,
                            ..Default::default()
                        }],
                        ..Default::default()
                    });
                }
            }
        }

        // ── 兜底：无 .hhc（或一条都读不到）→ 全部 .htm/.html 按路径
        //    大小写不敏感字典序（CHM 目录键序）。取舍见模块头。──
        if pages.is_empty() {
            let entries = chm
                .entries(EntrySel::NORMAL | EntrySel::FILES)
                .map_err(|e| DocError::corrupt(format!("CHM 目录读取失败: {e}")))?;
            let mut htmls: Vec<_> = entries
                .iter()
                .filter(|e| is_html(&e.path) && e.length > 0)
                .collect();
            htmls.sort_by(|a, b| cmp_ignore_ascii_case(&a.path, &b.path));
            for e in htmls {
                let bytes = chm
                    .read(e)
                    .map_err(|er| DocError::corrupt(format!("CHM 读取 {} 失败: {er}", e.path)))?;
                let html = crate::encoding::decode(bytes);
                pages.push(RawPage {
                    name: None,
                    items: vec![RawTextItem {
                        text: html,
                        ..Default::default()
                    }],
                    ..Default::default()
                });
            }
        }
        if pages.is_empty() {
            return Err(DocError::corrupt("CHM 内无 HTML 内容条目"));
        }

        // ── 图片条目提取（opt-in）：文档级资源放首页 images（CHM 无物理页
        //    概念；epub 五件套同款口径）──
        if self.ocr_images {
            let entries = chm
                .entries(EntrySel::NORMAL | EntrySel::FILES)
                .map_err(|e| DocError::corrupt(format!("CHM 目录读取失败: {e}")))?;
            let mut images = Vec::new();
            for e in &entries {
                if !is_image(&e.path) || e.length == 0 {
                    continue;
                }
                let bytes = chm
                    .read(e)
                    .map_err(|er| DocError::corrupt(format!("CHM 读取 {} 失败: {er}", e.path)))?;
                images.push(RawImage {
                    data: bytes,
                    ext: ext_of(&e.path),
                    ..Default::default()
                });
            }
            if let Some(first) = pages.first_mut() {
                first.images = images;
            }
        }

        Ok(RawDoc {
            pages,
            meta,
            needs_ocr: false,
        })
    }
}

/// 枚举兜底选 .hhc：无 #SYSTEM code 0 时，取路径段最少（根级 TOC 优先）、
/// 再按路径大小写不敏感字典序的第一个 .hhc 条目。
fn pick_hhc(chm: &mut ChmFile) -> Option<String> {
    let entries = chm.entries(EntrySel::NORMAL | EntrySel::FILES).ok()?;
    entries
        .iter()
        .filter(|e| ext_of(&e.path) == "hhc")
        .min_by(|a, b| {
            a.path
                .matches('/')
                .count()
                .cmp(&b.path.matches('/').count())
                .then_with(|| cmp_ignore_ascii_case(&a.path, &b.path))
        })
        .map(|e| e.path.clone())
}

/// CHM 插件：每章 HTML → 结构化块（一页一章）。
///
/// 取舍（相对 epub.rs 的文档内标题切节）：**不再切节**——.hhc 已给出章界
/// 与章名（CHM 的 spine），页名以 TOC 的 Name 为权威（不回落文档内首个
/// 标题）；纯跳转/空白章不成为页（epub"纯图空白页不成章"同口径，但挂了
/// 提取图片的首页保留）。
pub struct ChmPlugin;

impl FormatPlugin for ChmPlugin {
    fn id(&self) -> &str {
        "chm"
    }
    fn formats(&self) -> &[&str] {
        &["chm"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["chm.read"]
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
        let mut pages: Vec<Page> = Vec::new();
        for rp in &raw.pages {
            let Some(html) = rp.items.first().map(|i| i.text.as_str()) else {
                continue;
            };
            let blocks = parse_blocks(html);
            let has_images = !rp.images.is_empty();
            if blocks.iter().all(|b| b.content.is_empty()) && !has_images {
                continue; // 纯跳转/空白章不成为页
            }
            // 页文本 = 非空 content 以空行连接（Image 块 content 为空不进文本，
            // 与 HtmlPlugin/MobiPlugin 同口径）
            let text = blocks
                .iter()
                .filter(|b| !b.content.is_empty())
                .map(|b| b.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            // 首页 images（ocr_images 提取 + ocr-images 处理链回填）→ Page.images
            let images = rp
                .images
                .iter()
                .map(|im| crate::model::Image {
                    data: im.data.clone(),
                    bbox: im.bbox,
                    ocr_text: im.ocr_text.clone(),
                    ..Default::default()
                })
                .collect();
            pages.push(Page {
                name: rp.name.clone(),
                blocks,
                text,
                images,
                ..Default::default()
            });
        }
        let mut doc = Document {
            pages,
            ..Default::default()
        };
        for key in ["chm.title", "chm.default_topic", "chm.lcid"] {
            if let Some(v) = raw.meta.get(key) {
                doc.meta.extra.insert(key.to_string(), v.clone());
            }
        }
        doc
    }
}

/// 注册 CHM 后端与插件（默认不开图片提取）。
pub fn register_defaults(reg: &mut Registry) {
    register_with_ocr_images(reg, false);
}

/// 同 [`register_defaults`]，但后端带 `ocr_images` 开关（提取图片条目字节）。
pub fn register_with_ocr_images(reg: &mut Registry, ocr_images: bool) {
    let _ = reg.register_parse(Arc::new(ChmRawBackend { ocr_images }), None);
    reg.register_plugin(Arc::new(ChmPlugin));
}

// ── 测试：手工构造最小 CHM 金测（libchm 只读、无 writer） ───────────
//
// 未压缩段（space=0）条目不经 LZX 直读（libchm chm.rs L127-137），故按
// format.rs 的二进制布局编 ITSF v3 头（data_offset 显式，L126-133）+ ITSP
// v1 目录头（index_root=-1 走单 PMGL 叶，directory.rs L169）+ 单 PMGL 块
// （条目 = cword 路径长 + 路径 + cword space/start/length，format.rs
// L247-255；cword 为续位大端 base-128，L217-232）。
#[cfg(test)]
mod tests {
    use super::*;

    /// cword 编码：base-128 大端，除最后一字节外都带续位 0x80
    /// （libchm parse_cword，format.rs L217-232 的逆）。
    fn push_cword(mut v: u64, out: &mut Vec<u8>) {
        let mut groups = Vec::new();
        loop {
            groups.push((v & 0x7f) as u8);
            v >>= 7;
            if v == 0 {
                break;
            }
        }
        for (i, g) in groups.iter().enumerate().rev() {
            if i == 0 {
                out.push(*g);
            } else {
                out.push(*g | 0x80);
            }
        }
    }

    /// 构造最小未压缩 CHM：`[ITSF v3 0x60][内容区][ITSP 0x54 + PMGL 0x1000]`。
    /// 条目按存储序写入（PMGL 线性扫描，不要求排序）。
    fn build_chm(entries: &[(&str, &[u8])]) -> Vec<u8> {
        const BLOCK_LEN: usize = 0x1000;
        const ITSF_LEN: usize = 0x60;
        const ITSP_LEN: usize = 0x54;

        // 内容区：条目字节依次拼接，记起点
        let mut content = Vec::new();
        let mut starts = Vec::new();
        for (_, data) in entries {
            starts.push(content.len() as u64);
            content.extend_from_slice(data);
        }

        // PMGL 条目区
        let mut ents = Vec::new();
        for (i, (path, data)) in entries.iter().enumerate() {
            push_cword(path.len() as u64, &mut ents);
            ents.extend_from_slice(path.as_bytes());
            push_cword(0, &mut ents); // space = 0（未压缩段）
            push_cword(starts[i], &mut ents);
            push_cword(data.len() as u64, &mut ents);
        }
        assert!(
            ents.len() + 0x14 <= BLOCK_LEN,
            "测试件条目超出单 PMGL 块（真实件会分叶，构造件控制规模）"
        );
        let mut pmgl = vec![0u8; BLOCK_LEN];
        pmgl[0..4].copy_from_slice(b"PMGL");
        let free = (BLOCK_LEN - 0x14 - ents.len()) as u32;
        pmgl[4..8].copy_from_slice(&free.to_le_bytes());
        pmgl[0x10..0x14].copy_from_slice(&(-1i32).to_le_bytes()); // block_next = 链尾
        pmgl[0x14..0x14 + ents.len()].copy_from_slice(&ents);

        // ITSP v1：单叶（index_root=-1 → Directory::new 用 index_head=0）
        let mut dir = vec![0u8; ITSP_LEN];
        dir[0..4].copy_from_slice(b"ITSP");
        dir[4..8].copy_from_slice(&1u32.to_le_bytes());
        dir[8..12].copy_from_slice(&(ITSP_LEN as u32).to_le_bytes());
        dir[0x10..0x14].copy_from_slice(&(BLOCK_LEN as u32).to_le_bytes());
        dir[0x1c..0x20].copy_from_slice(&(-1i32).to_le_bytes());
        dir[0x20..0x24].copy_from_slice(&0i32.to_le_bytes());
        dir.extend_from_slice(&pmgl);

        // ITSF v3：data_offset 显式（0x60），目录紧随内容区
        let data_offset = ITSF_LEN as u64;
        let dir_offset = data_offset + content.len() as u64;
        let mut out = vec![0u8; ITSF_LEN];
        out[0..4].copy_from_slice(b"ITSF");
        out[4..8].copy_from_slice(&3u32.to_le_bytes());
        out[8..12].copy_from_slice(&(ITSF_LEN as u32).to_le_bytes());
        out[0x48..0x50].copy_from_slice(&dir_offset.to_le_bytes());
        out[0x50..0x58].copy_from_slice(&(dir.len() as u64).to_le_bytes());
        out[0x58..0x60].copy_from_slice(&data_offset.to_le_bytes());
        out.extend_from_slice(&content);
        out.extend_from_slice(&dir);
        out
    }

    /// 写测试件到 target/（mobi 后端同款落盘口径）。
    fn write_chm(dir_name: &str, entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("chm-{dir_name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("book.chm");
        std::fs::write(&path, build_chm(entries)).unwrap();
        path
    }

    /// HHW 风格 .hhc（大写标签 + 不闭合 param/LI + site properties + 纯分组节点）。
    fn hhc_markup(items: &[(&str, &str)]) -> String {
        let mut s = String::from(
            "<HTML>\n<BODY>\n<OBJECT type=\"text/site properties\">\n  \
             <param name=\"Window Styles\" value=\"0x800225\">\n</OBJECT>\n<UL>\n",
        );
        for (title, local) in items {
            s.push_str("  <LI> <OBJECT type=\"text/sitemap\">\n");
            s.push_str(&format!(
                "    <param name=\"Name\" value=\"{title}\">\n"
            ));
            if !local.is_empty() {
                s.push_str(&format!(
                    "    <param name=\"Local\" value=\"{local}\">\n"
                ));
            }
            s.push_str("    </OBJECT>\n");
        }
        s.push_str("</UL>\n</BODY>\n</HTML>");
        s
    }

    /// `/#SYSTEM` 件：code 2 默认页（ANSI NT）+ code 3 书名（UTF-16LE NT）
    /// + code 4 LCID（首个 DWORD）+ code 0 目录件名（ANSI NT）。
    fn system_bytes(title: &str, default_topic: &str, lcid: u32, contents: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&3u32.to_le_bytes()); // 版本（compat 1.1+）
        let entry = |code: u16, data: &[u8], out: &mut Vec<u8>| {
            out.extend_from_slice(&code.to_le_bytes());
            out.extend_from_slice(&(data.len() as u16).to_le_bytes());
            out.extend_from_slice(data);
        };
        let mut d = default_topic.as_bytes().to_vec();
        d.push(0);
        entry(2, &d, &mut out);
        let mut d: Vec<u8> = title.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        d.extend_from_slice(&0u16.to_le_bytes());
        entry(3, &d, &mut out);
        entry(4, &lcid.to_le_bytes(), &mut out);
        let mut d = contents.as_bytes().to_vec();
        d.push(0);
        entry(0, &d, &mut out);
        out
    }

    const Z1: &str =
        "<html><head><title>占位</title></head><body><h1>第十章 标题</h1><p>收尾正文。</p></body></html>";
    const A1: &str = "<html><body><h1>第一章 标题</h1><p>起点正文。</p></body></html>";
    const APP: &str = "<html><body><h2>附表</h2><ul><li>条目甲</li></ul></body></html>";
    const INDEX: &str = "<html><body><p>欢迎页（不在 TOC 里）。</p></body></html>";
    /// 8 字节 PNG 头（epub 五件套测试同款：只验提取，不解码）。
    const PNG_SIG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    /// 金测（构造真件走真读取路径）：.hhc 序定章（≠ 文件名字典序）、章名进
    /// page.name、/#SYSTEM 元数据三键、TOC 引用之外的条目不进阅读流。
    #[test]
    fn reads_hhc_order_titles_and_content_from_constructed_chm() {
        let hhc = hhc_markup(&[
            ("第十章 收尾", "z1.htm"),
            ("第一章 起点", "a1.htm"),
            ("附录", "app.htm#tables"),  // 锚点应剥掉
            ("第一章 重复锚点", "a1.htm#x"), // 同文件多锚点：按首次去重
            ("纯分组节点", ""),          // 无 Local：出局
        ]);
        let p = write_chm(
            "full",
            &[
                ("/#SYSTEM", system_bytes("测试手册", "index.htm", 0x0804, "toc.hhc").as_slice()),
                ("/toc.hhc", hhc.as_bytes()),
                ("/z1.htm", Z1.as_bytes()),
                ("/a1.htm", A1.as_bytes()),
                ("/app.htm", APP.as_bytes()),
                ("/index.htm", INDEX.as_bytes()),
            ],
        );
        let raw = ChmRawBackend::default().parse(p.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());

        // 元数据：/#SYSTEM code 3/2/4
        assert_eq!(raw.meta.get("chm.title").map(String::as_str), Some("测试手册"));
        assert_eq!(
            raw.meta.get("chm.default_topic").map(String::as_str),
            Some("index.htm")
        );
        assert_eq!(raw.meta.get("chm.lcid").map(String::as_str), Some("2052"));

        // 阅读序 = .hhc 文档序（z1 在前，文件名字典序会反过来）；
        // index.htm 不在 TOC → 不进；重复锚点去重 → 3 章
        assert_eq!(raw.pages.len(), 3, "{:?}", raw.pages.iter().map(|p| p.name.clone()).collect::<Vec<_>>());
        assert_eq!(raw.pages[0].name.as_deref(), Some("第十章 收尾"));
        assert_eq!(raw.pages[1].name.as_deref(), Some("第一章 起点"));
        assert_eq!(raw.pages[2].name.as_deref(), Some("附录"));
        assert!(raw.pages[0].items[0].text.contains("收尾正文。"));
        assert!(raw.pages[1].items[0].text.contains("起点正文。"));
        assert!(!raw.needs_ocr);
    }

    /// .hhc 解析单元口径（纯字符串）：文档序、site properties 与纯分组节点
    /// 过滤、锚点剥除、去重留给后端（这里只验提取）。
    #[test]
    fn hhc_parser_extracts_name_local_pairs_in_order() {
        let hhc = hhc_markup(&[
            ("甲 &amp; 乙", "a&amp;b.htm"), // 实体标题/路径
            ("纯节点", ""),
            ("丙", "./sub\\c.htm#frag"),
        ]);
        let toc = parse_hhc(&hhc);
        assert_eq!(
            toc,
            vec![
                TocEntry { title: "甲 & 乙".into(), path: "/a&b.htm".into() },
                TocEntry { title: "丙".into(), path: "/sub/c.htm".into() },
            ]
        );
    }

    /// Local 归一口径：反斜杠、锚点、%XX、./ 前缀、前导斜杠补齐。
    #[test]
    fn normalize_local_strips_fragment_backslash_and_percent() {
        assert_eq!(normalize_local("a\\b\\Page 1.htm#top"), "/a/b/Page 1.htm");
        assert_eq!(normalize_local("./x/y.htm"), "/x/y.htm");
        assert_eq!(normalize_local("a%20b.htm"), "/a b.htm");
        assert_eq!(normalize_local("/abs.htm"), "/abs.htm");
        assert_eq!(normalize_local("  "), "");
        // 非法 % 序列原样保留
        assert_eq!(normalize_local("a%2G.htm"), "/a%2G.htm");
    }

    /// /#SYSTEM TLV 单元口径：乱序 code、未知 code 跳过、残缺尾停表。
    #[test]
    fn system_meta_parses_title_default_topic_lcid_and_contents() {
        let mut bytes = system_bytes("手册", "idx.htm", 0x0409, "t.hhc");
        // 尾部追加未知 code（9）与越界残条（长度声明超出 EOF）：前者跳过、后者停表
        bytes.extend_from_slice(&9u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&[0xAB, 0xCD]);
        bytes.extend_from_slice(&7u16.to_le_bytes());
        bytes.extend_from_slice(&200u16.to_le_bytes());
        let meta = parse_system(&bytes);
        assert_eq!(meta.title, "手册");
        assert_eq!(meta.default_topic, "idx.htm");
        assert_eq!(meta.contents_file, "t.hhc");
        assert_eq!(meta.lcid, Some(0x0409));
    }

    /// #SYSTEM 串解码启发式：UTF-16LE（含终止对）与 ANSI（GBK 字节 /
    /// 纯 ASCII）两条路；GBK 命中依赖 `encoding` feature（未开则 lossy）。
    #[test]
    fn system_string_utf16le_and_ansi_paths() {
        // UTF-16LE + NUL 终止对
        let mut utf16: Vec<u8> = "My Book".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        utf16.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(system_string(&utf16), "My Book");
        // 中文 UTF-16LE
        let mut utf16: Vec<u8> = "测试手册".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        utf16.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(system_string(&utf16), "测试手册");
        // ANSI 纯 ASCII（偶长但无终止对、高位字节不全零 → 不误判 UTF-16）
        assert_eq!(system_string(b"Plain Title\0"), "Plain Title");
        // ANSI GBK"中文"（无 BOM 非 UTF-8）：开 encoding 走 GB18030 链
        let gbk = [0xD6, 0xD0, 0xCE, 0xC4];
        let got = system_string(&gbk);
        #[cfg(feature = "encoding")]
        assert_eq!(got, "中文", "GBK 应经统一链解码");
        #[cfg(not(feature = "encoding"))]
        assert!(!got.contains("中文"), "未开 encoding 应保持 lossy");
        // 短/残缺输入不 panic
        assert_eq!(system_string(&[]), "");
        assert_eq!(system_string(&[0x41]), "A");
    }

    /// 兜底序：无 .hhc 时全部 .htm/.html 按路径大小写不敏感字典序，
    /// 非扩展名条目（图/HHK）出局，章名缺省为 None。
    #[test]
    fn falls_back_to_sorted_html_without_hhc() {
        let p = write_chm(
            "fallback",
            &[
                ("/z1.htm", Z1.as_bytes()),
                ("/a1.html", A1.as_bytes()),
                ("/index.htm", INDEX.as_bytes()),
                ("/Logo.PNG", PNG_SIG),
                ("/idx.hhk", b"<html></html>"),
            ],
        );
        let raw = ChmRawBackend::default().parse(p.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());

        let names: Vec<Option<String>> = raw.pages.iter().map(|p| p.name.clone()).collect();
        assert_eq!(raw.pages.len(), 3);
        assert_eq!(
            raw.pages
                .iter()
                .map(|p| p.items[0].text.contains("起点正文。"))
                .collect::<Vec<_>>(),
            vec![true, false, false],
            "a1.html 应排最前（大小写不敏感字典序）"
        );
        assert!(names.iter().all(|n| n.is_none()));
    }

    /// 图片条目 opt-in：默认不提取；开关打开进首页 images（非图片扩展出局）。
    #[test]
    fn ocr_images_off_by_default_and_extracts_when_enabled() {
        let hhc = hhc_markup(&[("甲", "a1.htm")]);
        let sys = system_bytes("t", "a1.htm", 0x0804, "toc.hhc");
        let entries = [
            ("/#SYSTEM", sys.as_slice()),
            ("/toc.hhc", hhc.as_bytes()),
            ("/a1.htm", A1.as_bytes()),
            ("/img/logo.png", PNG_SIG),
            ("/img/icon.ico", &[0, 0, 1, 0][..]), // ico 不在白名单
        ];
        let p = write_chm("imgs", &entries);

        let raw_off = ChmRawBackend::default().parse(p.to_str().unwrap()).unwrap();
        assert!(raw_off.pages[0].images.is_empty());

        let raw_on = ChmRawBackend { ocr_images: true }
            .parse(p.to_str().unwrap())
            .unwrap();
        assert_eq!(raw_on.pages[0].images.len(), 1);
        assert_eq!(raw_on.pages[0].images[0].ext, "png");
        assert_eq!(&raw_on.pages[0].images[0].data[..4], &PNG_SIG[..4]);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    /// 组装口径：一章一页、块化（head/title 跳过）、章名保持 TOC Name、
    /// 元数据透传 meta.extra。
    #[test]
    fn assemble_blockifies_chapters_and_carries_meta() {
        let hhc = hhc_markup(&[("第十章 收尾", "z1.htm"), ("附录", "app.htm")]);
        let p = write_chm(
            "asm",
            &[
                ("/#SYSTEM", system_bytes("测试手册", "z1.htm", 0x0804, "toc.hhc").as_slice()),
                ("/toc.hhc", hhc.as_bytes()),
                ("/z1.htm", Z1.as_bytes()),
                ("/app.htm", APP.as_bytes()),
            ],
        );
        let raw = ChmRawBackend::default().parse(p.to_str().unwrap()).unwrap();
        let doc = ChmPlugin.assemble(&raw);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());

        assert_eq!(doc.pages.len(), 2);
        assert_eq!(doc.pages[0].name.as_deref(), Some("第十章 收尾"));
        use crate::model::BlockKind;
        let kinds: Vec<BlockKind> = doc.pages[0].blocks.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, vec![BlockKind::Heading, BlockKind::Paragraph]);
        assert_eq!(doc.pages[0].text, "第十章 标题\n\n收尾正文。");
        // 附录页：h2 标题 + 列表
        let kinds: Vec<BlockKind> = doc.pages[1].blocks.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, vec![BlockKind::Heading, BlockKind::List]);
        assert_eq!(doc.meta.extra.get("chm.title").map(String::as_str), Some("测试手册"));
        assert_eq!(doc.meta.extra.get("chm.lcid").map(String::as_str), Some("2052"));
    }

    /// 非 CHM 垃圾件 → corrupt（ITSF 头校验由 libchm 判）。
    #[test]
    fn garbage_is_corrupt() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("chm-garbage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.chm");
        std::fs::write(&path, b"this is definitely not a compiled html help file......").unwrap();
        let err = ChmRawBackend::default().parse(path.to_str().unwrap()).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(err.code.as_str(), "corrupt");
    }

    /// 纯图片件（无任何 .htm/.html/.hhc）→ corrupt。
    #[test]
    fn image_only_chm_is_corrupt() {
        let p = write_chm("imgonly", &[("/logo.png", PNG_SIG)]);
        let err = ChmRawBackend::default().parse(p.to_str().unwrap()).unwrap_err();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(err.message.contains("无 HTML"));
    }

    /// 端到端：引擎按 ITSF/.chm 路由 → chm 插件组装（需 router 增量接线）。
    #[test]
    fn engine_routes_chm_end_to_end() {
        let hhc = hhc_markup(&[("第一章 起点", "a1.htm")]);
        let p = write_chm(
            "engine",
            &[
                ("/#SYSTEM", system_bytes("测试手册", "a1.htm", 0x0804, "toc.hhc").as_slice()),
                ("/toc.hhc", hhc.as_bytes()),
                ("/a1.htm", A1.as_bytes()),
            ],
        );
        let mut reg = Registry::new();
        register_defaults(&mut reg);
        let doc = crate::DocumentEngine::new(reg).parse(p.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(p.parent().unwrap());

        assert_eq!(doc.meta.file_type, "chm");
        assert_eq!(doc.meta.page_count, 1);
        assert_eq!(doc.pages[0].name.as_deref(), Some("第一章 起点"));
        assert_eq!(doc.meta.extra.get("chm.title").map(String::as_str), Some("测试手册"));
    }

    /// 真实语料金测：SVN Book 官方 CHM 构建（《Version Control with
    /// Subversion》，CC-BY-2.0，svnbook.red-bean.com；取自 EvotecIT/OfficeIMO
    /// 测试件镜像，sha256 `6fef835e…a409f94d`，900KB）。真实 HHW 编译件：
    /// MSCompressed LZX 压缩段 + 真实 .hhc/#SYSTEM——覆盖构造件到不了的
    /// 压缩路径（构造件只能编未压缩段）。件缺失（打包剥离二进制件）时
    /// 跳过不破 CI；新样例放入 tests/fixtures/chm/ 即纳入逐件断言：
    /// 打开成功 + 至少一章 + 正文有非空页。
    #[test]
    fn real_corpus_golden() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/chm");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("跳过：tests/fixtures/chm/ 不存在（无真实语料件）");
            return;
        };
        let mut checked = 0;
        for e in entries.flatten() {
            let path = e.path();
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if !ext.eq_ignore_ascii_case("chm") {
                continue;
            }
            let raw = ChmRawBackend::default()
                .parse(path.to_str().unwrap())
                .unwrap_or_else(|e| panic!("{} 打开失败: {e}", path.display()));
            assert!(!raw.pages.is_empty(), "{} 应至少一章", path.display());
            let doc = ChmPlugin.assemble(&raw);
            assert!(
                doc.pages.iter().any(|p| !p.text.is_empty()),
                "{} 正文应有非空页",
                path.display()
            );
            checked += 1;
        }
        if checked == 0 {
            eprintln!("跳过：tests/fixtures/chm/ 内没有 .chm 样例（无真实语料件）");
        }
    }
}
