// rars 0.12.3（bitplane，Apache-2.0）与 sevenz-rust2 0.24（MIT，docforge 侧仅
// 以依赖形式引用，见 THIRD_PARTY_NOTICES.md 登记口径）；本文件为 docforge 原生实现。
//! 压缩包后端（feature = "archive"）：zip / rar / 7z 容器 → 内层文本多页合并文档。
//!
//! # 容器语义（与 fb2.zip / cbz 的分工）
//!
//! 入口是压缩包、内层是**任意支持格式**——与 `.fb2.zip`（内层固定单个 .fb2，
//! 语义绑死 fb2）和 cbz（内层固定图片页）是三种不同的 ZIP 容器语义。网文场景
//! 常见整本书 zip 打包（若干 txt 分章，中文编码混杂），rar/7z 亦有分布。
//!
//! # 递归边界（定案：不做"委托引擎递归解析"）
//!
//! [`ParseCapability`] 拿不到引擎引用，无法把内层文档递回 `engine.parse`；
//! 若递归调引擎会形成"容器 → 容器"解析环，且 pdf/epub 等内层各自的依赖链会把
//! archive feature 拖成全格式依赖。故采用**多页合并的简单形态**：
//!
//! * 内层**文本类**（txt/md/markdown/csv/html/htm）：解出字节 →
//!   [`crate::encoding::decode`] 解码（BOM/UTF-16/chardetng/GB18030/Big5 一条链）
//!   → 按空行切段 → 每文件一页（`RawPage.name` = 包内路径），页序按文件名
//!   自然排序（`第2章` < `第10章`，与 cbz 同一算法）；
//! * 内层**已知二进制文档格式**（pdf/epub/docx/…）：不解析，记 warning
//!   「请解包后单独解析」（与文本页共存时 doc 照常产出；全为二进制时整包报
//!   corrupt，错误信息列出格式）；
//! * zip 内层的 zip：递归展开**一层**——深度上限 2（外层=1，内层=2），第 3 层
//!   不再展开、记 warning（防 zip 炸弹）；
//! * rar/7z 内层压缩包**不递归**（记 warning）：内层 zip 在内存里用 zip crate
//!   一次 Cursor 即可解开，而内层 rar 需 rars 会话、内层 7z 需落盘临时目录，
//!   三种容器各自递归会让边界状态翻倍，收益（rar 套 rar 的网文包）几乎不存在。
//!
//! # 选型（2026-11 核实，报告行号依据）
//!
//! * zip：zip 9.3（工程已有，docx/epub/cbz/fb2 共用依赖）；
//! * rar：[`rars`] 0.12.3（bitplane）——**纯 Rust**（`--no-default-features`
//!   锁定依赖仅 aho-corasick + crc32fast；无 libunrar/unrar-sys，
//!   `unsafe_code = "forbid"`；MSRV 1.89；覆盖 RAR 1.3 → RAR 7 读路径）。
//!   encryption feature 未开，加密件报 `FeatureDisabled`/`PasswordRequired`/
//!   `BadPassword` → 映射 `encrypted`；
//! * 7z：[`sevenz_rust2`] 0.24——维护中的纯 Rust fork（原版 sevenz-rust 已
//!   RUSTSEC-2026-0246 弃维删库；fork 活跃，CVE-2026-61725 解压路径穿越修复于
//!   0.21.1，锁 0.24+）。只开 `util`（`decompress_file`）：该库无条目级内存
//!   读取接口，解压到临时目录即用即删是它的唯一形态（pdftoppm 同款模式）。
//!
//! # 路由约定（router.rs 增量，见报告）
//!
//! `.zip`（非 `.fb2.zip`，cbz 扩展名先行）→ `"zip"`；`.rar`/`.7z` 按扩展名直连
//! （两者无稳定 magic 判定口径之外的需求，后端内部再按 magic 复核容器类型）。
//! 内层全是图片的 zip 会被本后端以 corrupt + 疑似 CBZ 文案拒绝（应走 cbz）。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{DocError, ErrorCode, Result};
use crate::model::{Block, BlockKind, BlockSource, Document, Page};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc, RawPage, RawTextItem};
use crate::registry::Registry;

/// 内层文本条目扩展名（解出内容按空行切段拼为多页；编码走
/// [`crate::encoding::decode`]）。md/html/csv 不做语法解析——定案的简化形态
/// （避免容器后端再拖 markdown/html 语法依赖）。
const TEXT_EXTS: [&str; 6] = ["txt", "md", "markdown", "csv", "html", "htm"];
/// 已知二进制文档格式：记 warning「请解包后单独解析」，不解析（防引擎递归环）
const DOC_EXTS: [&str; 13] = [
    "pdf", "epub", "docx", "xlsx", "pptx", "odt", "ods", "odp", "fb2", "mobi", "djvu", "cbz",
    "rtf",
];
/// 图片条目扩展名（与 cbz.rs 同一白名单）：不计入文本页；整包全图 → 疑似 CBZ
const IMAGE_EXTS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "gif", "bmp"];
/// 嵌套深度上限：外层容器 = 1，内层 zip = 2（"递归一层"）；第 3 层不再展开。
/// 防 zip 炸弹：嵌套本身是免费的放大器，深度闸是最便宜的截断点。
const MAX_DEPTH: usize = 2;

// ---------------------------------------------------------------------------
// 条目分类
// ---------------------------------------------------------------------------

/// 单个条目的处置类别（按包内路径的扩展名判定）。
#[derive(Debug, Clone, PartialEq)]
enum EntryClass {
    /// 文本条目：解出内容成为一页
    Text,
    /// 内层 zip：深度允许则递归展开
    InnerZip,
    /// 内层 rar/7z：不递归，记 warning
    InnerOtherArchive,
    /// 已知二进制文档格式：记 warning
    Doc(&'static str),
    /// 图片条目：跳过（计数，供"疑似 CBZ"判定）
    Image,
    /// 其它（无扩展名 / 未知二进制 / 元数据垃圾）：静默跳过
    Other,
}

fn classify(name: &str) -> EntryClass {
    let Some(ext) = ext_of(name) else {
        return EntryClass::Other;
    };
    if ext == "zip" {
        return EntryClass::InnerZip;
    }
    if ext == "rar" || ext == "7z" {
        return EntryClass::InnerOtherArchive;
    }
    if TEXT_EXTS.iter().any(|e| *e == ext) {
        return EntryClass::Text;
    }
    if let Some(e) = DOC_EXTS.iter().copied().find(|e| *e == ext) {
        return EntryClass::Doc(e);
    }
    if IMAGE_EXTS.iter().any(|e| *e == ext) {
        return EntryClass::Image;
    }
    EntryClass::Other
}

/// 路径小写扩展名（取最后一段 `.` 之后；无 `.` 或 `.` 在末尾 → None）。
fn ext_of(name: &str) -> Option<String> {
    let file = name.rsplit('/').next().unwrap_or(name);
    let dot = file.rfind('.')?;
    if dot + 1 >= file.len() {
        return None;
    }
    Some(file[dot + 1..].to_ascii_lowercase())
}

/// 该类别是否需要读出条目字节（Doc/Image/Other 只看扩展名，不解码内容——
/// 大体积二进制（内嵌 PDF 的 zip）不进内存）。
fn needs_payload(c: &EntryClass) -> bool {
    matches!(c, EntryClass::Text | EntryClass::InnerZip)
}

/// 扫描统计：整包无可解析文本时，错误信息按内容分型（空包 / 全图片 / 仅二进制）。
#[derive(Default)]
struct ScanStats {
    images: usize,
    docs: Vec<String>,
}

/// 解包收敛后的一个文本条目（页序输入）。
struct ArchiveEntry {
    name: String,
    data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// 三种容器的条目收集（zip 内存展开 / rar 库读取 / 7z 临时目录）
// ---------------------------------------------------------------------------

/// zip 容器（含内层 zip 递归）：字节进内存逐条目收敛。
///
/// `depth` 从 1（外层）起；内层 zip 递归时 +1，超过 [`MAX_DEPTH`] 记 warning
/// 不展开。`prefix` 是内层条目的页名前缀（`"内层.zip/"`），避免多内层包页名互撞。
fn collect_zip(
    data: &[u8],
    depth: usize,
    prefix: &str,
    flat: &mut Vec<ArchiveEntry>,
    warns: &mut Vec<String>,
    stats: &mut ScanStats,
) -> Result<()> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data))
        .map_err(|e| DocError::corrupt(format!("ZIP 打开失败（非 ZIP）: {e}")))?;
    for i in 0..zip.len() {
        let mut f = zip
            .by_index(i)
            .map_err(|e| map_zip_err(e, format!("ZIP 条目 {i} 读取失败")))?;
        if f.is_dir() {
            continue;
        }
        // 条目名非 UTF-8（UTF-8 标志位异常）的跳过——cbz 同口径，不致命
        let Some(name) = f.name().ok().map(|n| n.into_owned()) else {
            continue;
        };
        let full = format!("{prefix}{name}");
        match classify(&full) {
            c if needs_payload(&c) => {
                let mut buf = Vec::new();
                // ZipFile 的 Read 实现返回 io::Error（解密/解码失败在此浮出）
                f.read_to_end(&mut buf).map_err(|e| {
                    DocError::corrupt(format!("ZIP 条目 {name} 解压失败: {e}"))
                })?;
                absorb(&c, &full, buf, depth, flat, warns, stats);
            }
            EntryClass::Doc(ext) => {
                stats.docs.push(ext.to_string());
                warns.push(format!(
                    "压缩包含二进制格式 {ext}（{full}），请解包后单独解析"
                ));
            }
            EntryClass::Image => stats.images += 1,
            _ => {}
        }
    }
    Ok(())
}

/// 收敛单个有字节的条目：文本入页序，内层 zip 按深度递归或记 warning。
fn absorb(
    class: &EntryClass,
    name: &str,
    data: Vec<u8>,
    depth: usize,
    flat: &mut Vec<ArchiveEntry>,
    warns: &mut Vec<String>,
    stats: &mut ScanStats,
) {
    match class {
        EntryClass::Text => flat.push(ArchiveEntry {
            name: name.to_string(),
            data,
        }),
        EntryClass::InnerZip => {
            if depth < MAX_DEPTH {
                let mut inner = Vec::new();
                let mut inner_stats = ScanStats::default();
                let res = collect_zip(
                    &data,
                    depth + 1,
                    &format!("{name}/"),
                    &mut inner,
                    warns,
                    &mut inner_stats,
                );
                match res {
                    Ok(()) => {
                        flat.extend(inner);
                        stats.images += inner_stats.images;
                        stats.docs.extend(inner_stats.docs);
                    }
                    // 内层包损坏不致命：整包继续，警告注明
                    Err(e) => warns.push(format!("内层压缩包 {name} 解包失败，已跳过：{e}")),
                }
            } else {
                warns.push(format!(
                    "压缩包嵌套超过深度上限（{MAX_DEPTH}），已跳过：{name}"
                ));
            }
        }
        _ => unreachable!("absorb 只接 Text/InnerZip（needs_payload 保证）"),
    }
}

/// rars 错误 → 统一错误码：密码类 → encrypted；未启用的能力（encryption
/// feature 未开时的加密件表现）→ encrypted；其余 → corrupt。
fn map_rar_err(e: rars::Error) -> DocError {
    match e.kind() {
        rars::ErrorKind::PasswordRequired | rars::ErrorKind::BadPassword => DocError {
            code: ErrorCode::Encrypted,
            message: format!("RAR 已加密（密码保护）：{e}"),
            attempts: vec![],
        },
        rars::ErrorKind::UnsupportedFeature => DocError {
            code: ErrorCode::Encrypted,
            message: format!("RAR 需要未启用的能力（多为加密件）：{e}"),
            attempts: vec![],
        },
        _ => DocError::corrupt(format!("RAR 解析失败：{e}")),
    }
}

/// rar 容器：rars 库直读（无临时文件、无外部命令）。成员枚举与批量解码
/// 用同一会话（solid 包的前缀解码只做一遍）。
///
/// 成员名按字节取 lossy UTF-8（RAR 4.x 非 Unicode 名是 OEM 编码，lossy 只
/// 影响页名展示，不影响内容）；目录 / 重定向（symlink）跳过。
fn collect_rar(
    path: &str,
    flat: &mut Vec<ArchiveEntry>,
    warns: &mut Vec<String>,
    stats: &mut ScanStats,
) -> Result<()> {
    let archive = rars::ArchiveReader::read_path(path).map_err(map_rar_err)?;
    let mut wanted: Vec<(usize, String)> = Vec::new();
    for (idx, member) in archive.member_refs().enumerate() {
        if member.is_directory() || member.is_redirection() {
            continue;
        }
        let name = String::from_utf8_lossy(member.name_bytes()).into_owned();
        match classify(&name) {
            c if needs_payload(&c) => wanted.push((idx, name)),
            EntryClass::Doc(ext) => {
                stats.docs.push(ext.to_string());
                warns.push(format!(
                    "压缩包含二进制格式 {ext}（{name}），请解包后单独解析"
                ));
            }
            EntryClass::Image => stats.images += 1,
            _ => {}
        }
    }
    if wanted.is_empty() {
        return Ok(());
    }
    let indices: Vec<usize> = wanted.iter().map(|(i, _)| *i).collect();
    let payloads = archive
        .read_members_at(&indices, None)
        .map_err(map_rar_err)?;
    for ((_, name), payload) in wanted.into_iter().zip(payloads) {
        // None = 目录/重定向/缺失（枚举时已过滤，理论不达；防御性跳过）
        let Some(data) = payload else { continue };
        // 枚举时已按 needs_payload 过滤，这里只可能是 Text / InnerZip
        let class = classify(&name);
        absorb(&class, &name, data, 1, flat, warns, stats);
    }
    Ok(())
}

/// sevenz-rust2 错误 → 统一错误码。
fn map_7z_err(e: sevenz_rust2::Error) -> DocError {
    match e {
        sevenz_rust2::Error::PasswordRequired | sevenz_rust2::Error::MaybeBadPassword(_) => {
            DocError {
                code: ErrorCode::Encrypted,
                message: format!("7z 已加密（密码保护）：{e}"),
                attempts: vec![],
            }
        }
        _ => DocError::corrupt(format!("7z 解压失败：{e}")),
    }
}

/// 7z 容器：解压到临时目录 → 遍历收敛 → 即用即删（pdftoppm 同款模式）。
/// sevenz-rust2 无条目级内存读取接口，目录级解压是它的唯一形态；非文本条目
/// 虽会落盘但不进内存（只看扩展名分类）。
fn collect_sevenz(
    path: &Path,
    flat: &mut Vec<ArchiveEntry>,
    warns: &mut Vec<String>,
    stats: &mut ScanStats,
) -> Result<()> {
    let dir = std::env::temp_dir().join(format!(
        "docforge-7z-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| DocError::corrupt(format!("7z 临时目录创建失败：{e}")))?;
    let result = sevenz_rust2::decompress_file(path, &dir);
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(map_7z_err(e));
    }
    // 栈式遍历（不递归调用栈），相对路径即包内条目名
    let mut stack: Vec<PathBuf> = vec![dir.clone()];
    let mut walk_err: Option<DocError> = None;
    'outer: while let Some(cur) = stack.pop() {
        let entries = match std::fs::read_dir(&cur) {
            Ok(it) => it,
            Err(e) => {
                walk_err = Some(DocError::corrupt(format!("7z 解压目录遍历失败：{e}")));
                break;
            }
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            // 相对名（条目名）；文件系统非法 UTF-8 名 lossy（页名展示口径同 rar）
            let rel = p
                .strip_prefix(&dir)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            match classify(&rel) {
                c if needs_payload(&c) => {
                    match std::fs::read(&p) {
                        Ok(data) => absorb(&c, &rel, data, 1, flat, warns, stats),
                        Err(e) => {
                            walk_err = Some(DocError::corrupt(format!(
                                "7z 条目 {rel} 读取失败：{e}"
                            )));
                            break 'outer;
                        }
                    }
                }
                EntryClass::Doc(ext) => {
                    stats.docs.push(ext.to_string());
                    warns.push(format!(
                        "压缩包含二进制格式 {ext}（{rel}），请解包后单独解析"
                    ));
                }
                EntryClass::Image => stats.images += 1,
                _ => {}
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    match walk_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// 容器识别与页装配
// ---------------------------------------------------------------------------

/// 容器类型：按 magic 复核（扩展名路由在前，后端防伪装件）。
#[derive(Debug, Clone, Copy, PartialEq)]
enum ContainerKind {
    Zip,
    Rar,
    SevenZ,
}

fn detect_kind(head: &[u8], ext: &str) -> ContainerKind {
    if head.starts_with(b"PK\x03\x04") {
        ContainerKind::Zip
    } else if head.starts_with(b"Rar!") {
        ContainerKind::Rar
    } else if head.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        ContainerKind::SevenZ
    } else {
        // 无 magic：按扩展名兜底（.zip 交由打开器报 corrupt，cbz 同口径）
        match ext {
            "rar" => ContainerKind::Rar,
            "7z" => ContainerKind::SevenZ,
            _ => ContainerKind::Zip,
        }
    }
}

/// zip 错误 → 统一错误码：需要密码（ZipCrypto/AES 加密条目）→ encrypted。
fn map_zip_err(e: zip::result::ZipError, what: String) -> DocError {
    let s = e.to_string();
    if s.to_lowercase().contains("password") {
        DocError {
            code: ErrorCode::Encrypted,
            message: format!("{what}（已加密）：{s}"),
            attempts: vec![],
        }
    } else {
        DocError::corrupt(format!("{what}: {s}"))
    }
}

/// 收敛结果 → RawDoc：页序按文件名自然排序（`第2章` < `第10章`）。
/// 整包无文本页时按内容分型报 corrupt（空包 / 全图片疑似 CBZ / 仅二进制文档）。
fn build_raw(mut flat: Vec<ArchiveEntry>, stats: ScanStats, warns: Vec<String>) -> Result<RawDoc> {
    if flat.is_empty() {
        return Err(if stats.images > 0 {
            DocError::corrupt(format!(
                "压缩包内没有文本条目（有 {} 个图片条目，疑似 CBZ 漫画包，请按 .cbz 走 cbz 后端）",
                stats.images
            ))
        } else if !stats.docs.is_empty() {
            DocError::corrupt(format!(
                "压缩包内没有文本条目（仅二进制文档格式 {}，请解包后单独解析）",
                stats.docs.join("、")
            ))
        } else {
            DocError::corrupt("压缩包内没有可解析的文本条目")
        });
    }
    flat.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    let pages: Vec<RawPage> = flat
        .into_iter()
        .map(|e| {
            // 编码探测横切能力：BOM/UTF-16 → strict UTF-8 →（encoding feature）
            // chardetng/GB18030/Big5 → lossy（与 txt 直读同一链）
            let text = crate::encoding::decode(e.data);
            let items: Vec<RawTextItem> = crate::plugins::text::split_paragraphs(&text)
                .into_iter()
                .map(|para| RawTextItem {
                    text: para,
                    source: BlockSource::Native,
                    ..Default::default()
                })
                .collect();
            RawPage {
                name: Some(e.name),
                items,
                ..Default::default()
            }
        })
        .collect();
    let mut meta = std::collections::HashMap::new();
    if !warns.is_empty() {
        // pdf.rs/djvu.rs 同口径：warning 进 raw.meta，engine.assemble 搬到
        // doc.meta.extra["warnings"]
        meta.insert("warnings".to_string(), warns.join("; "));
    }
    Ok(RawDoc {
        pages,
        meta,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// 解析能力 + 插件
// ---------------------------------------------------------------------------

/// 压缩包原始读取后端（zip 内存展开 / rar 库读取 / 7z 临时目录）。
#[derive(Default)]
pub struct ArchiveRawBackend;

impl ParseCapability for ArchiveRawBackend {
    fn id(&self) -> &str {
        "archive.read"
    }
    fn formats(&self) -> &[&str] {
        &["zip", "rar", "7z"]
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let head = std::fs::read(path)
            .map_err(|e| DocError::corrupt(format!("文件不存在或不可读 {path}: {e}")))?;
        let head = &head[..head.len().min(8)];
        let ext = ext_of(path).unwrap_or_default();
        let kind = detect_kind(head, &ext);
        let mut flat = Vec::new();
        let mut warns = Vec::new();
        let mut stats = ScanStats::default();
        match kind {
            ContainerKind::Zip => {
                let bytes = std::fs::read(path)
                    .map_err(|e| DocError::corrupt(format!("文件不存在或不可读 {path}: {e}")))?;
                collect_zip(&bytes, 1, "", &mut flat, &mut warns, &mut stats)?;
            }
            ContainerKind::Rar => collect_rar(path, &mut flat, &mut warns, &mut stats)?,
            ContainerKind::SevenZ => {
                collect_sevenz(Path::new(path), &mut flat, &mut warns, &mut stats)?
            }
        }
        build_raw(flat, stats, warns)
    }
}

/// 压缩包插件：RawDoc（每文本文件一页）→ IR（每段一个 Paragraph 块）。
#[derive(Default)]
pub struct ArchivePlugin;

impl FormatPlugin for ArchivePlugin {
    fn id(&self) -> &str {
        "archive"
    }
    fn formats(&self) -> &[&str] {
        &["zip", "rar", "7z"]
    }
    fn default_parse_chain(&self) -> Vec<&'static str> {
        vec!["archive.read"]
    }
    fn default_process_chain(&self) -> Vec<&'static str> {
        // 无内建处理链：lang-detect 等按需全局追加（engine 侧注册即生效）
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
                    .map(|i| Block {
                        kind: BlockKind::Paragraph,
                        content: i.text.clone(),
                        source: i.source,
                        confidence: i.confidence,
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

/// 注册压缩包后端（解析能力 + 插件）。
pub fn register_defaults(reg: &mut Registry) {
    let _ = reg.register_parse(Arc::new(ArchiveRawBackend), None);
    reg.register_plugin(Arc::new(ArchivePlugin));
}

// ---------------------------------------------------------------------------
// 自然排序（数字段按值比较；`第2章` < `第10章`）——cbz.rs 同款算法
// ---------------------------------------------------------------------------

/// 把名字切成 (非数字段, 数字值) 序列供比较。
fn natural_key(name: &str) -> Vec<(String, Option<u64>)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut digits = String::new();
    for c in name.chars() {
        if c.is_ascii_digit() {
            if !buf.is_empty() {
                out.push((std::mem::take(&mut buf), None));
            }
            digits.push(c);
        } else {
            if !digits.is_empty() {
                let v = digits.parse::<u64>().unwrap_or(0);
                out.push((String::new(), Some(v)));
                digits.clear();
            }
            buf.push(c.to_ascii_lowercase());
        }
    }
    if !buf.is_empty() {
        out.push((buf, None));
    }
    if !digits.is_empty() {
        let v = digits.parse::<u64>().unwrap_or(0);
        out.push((String::new(), Some(v)));
    }
    out
}

/// 自然序比较：数字段按值、非数字段按字典；短前缀优先。
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (ka, kb) = (natural_key(a), natural_key(b));
    let mut ia = ka.iter();
    let mut ib = kb.iter();
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some((sa, va)), Some((sb, vb))) => {
                let ord = match (va, vb) {
                    (Some(x), Some(y)) => x.cmp(y),
                    (None, None) => sa.cmp(sb),
                    // 数字段 < 文本段（数字开头的排前，与直觉一致）
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 测试（真件：zip 运行时构造；rar/7z 用真字节 fixture（无 write feature，
// fixture 由 rars/sevenz-rust2 的写路径预生成并内嵌 hex））
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// 测试真 rar fixture（RAR5 stored，成员：第一章.txt / 第二章.txt，
    /// rars Builder 生成后内嵌；读路径 default-features 即可解）
    const RAR_FIXTURE_HEX: &str = "526172211A070100C51A3332030100009B5254561103061C041C0061B7A10C00000452415253726172732F302E31322E330A6D61726B65722D736368656D613D310A2E0B0B463E02032332043220B3996CC500000DE7ACACE4B880E7ABA02E747874220200BB538C10B1283F1B8FFAF292797A3EDFD186A96BB1F239A4E90849D24D8DB06EE9A38EE8B5B7E4BA8EE99D92E8908DE4B98BE69CABE380820A0AE6B5AAE68890E4BA8EE5BEAEE6BE9CE4B98BE997B4E380822DA6B2873E020323240424200DCD3DCF00000DE7ACACE4BA8CE7ABA02E7478742202000CA5FFDD3B16CDA6D97F5E8B42593730894E85D30FEA8E2F5816442368BCD248E5B091E5B9B4E68EA8E997A8E8808CE587BAEFBC8CE5A4A9E59CB0E4B880E799BDE3808219B23A3503050000";

    /// 测试真 7z fixture（两成员：第一章.txt / 第二章.txt，sevenz-rust2 写路径
    /// 预生成；lib 默认 LZMA/copy 解码即可读）
    const SEVENZ_FIXTURE_HEX: &str = "377ABCAF271C0004D00363C7380000000000000059000000000000006C649287010017E9A38EE8B5B7E4BA8EE99D92E8908DE4B98BE69CABE3808200010017E6B5AAE68890E4BA8EE5BEAEE6BE9CE4B98BE997B4E38082000104060002091C1C0A0125A534C1B1B46F0F00070B0200012121011601212101160C181800080A010CA6A21B7BAD5721000005021121002C7B004EE07A2E0074007800740000002C7B8C4EE07A2E0074007800740000000000";

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("archive-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 构造 zip 文件：给定 (条目名, 字节) 列表。
    fn build_zip(dir: &Path, name: &str, entries: &[(&str, Vec<u8>)]) -> PathBuf {
        let path = dir.join(name);
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (n, data) in entries {
            zw.start_file(*n, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
        path
    }

    fn page_names(raw: &RawDoc) -> Vec<&str> {
        raw.pages.iter().map(|p| p.name.as_deref().unwrap()).collect()
    }

    /// 三个中文 txt（含 GBK 内码件）各成一页 + 自然页序 + 编码探测生效。
    #[test]
    fn zip_with_multiple_txt_becomes_multi_page() {
        let dir = tmpdir("multi");
        // "终章。\n\n全书完。" 的 GBK 编码（非 UTF-8、无 BOM，编码链验证件）
        let gbk = hex_decode("D6D5D5C2A1A30A0AC8ABCAE9CDEAA1A3");
        let p = build_zip(
            &dir,
            "book.zip",
            &[
                // 写入顺序故意乱序，页序由自然排序决定（1 < 2 < 10；
                // ASCII 序会把 `第10章` 排在 `第2章` 前）
                ("第10章.txt", gbk),
                ("第1章.txt", "风起于青萍之末。\n\n浪成于微澜之间。".as_bytes().to_vec()),
                ("第2章.txt", "少年推门而出。".as_bytes().to_vec()),
            ],
        );
        let raw = ArchiveRawBackend.parse(p.to_str().unwrap()).unwrap();
        assert_eq!(page_names(&raw), vec!["第1章.txt", "第2章.txt", "第10章.txt"]);

        // 第 1 页：空行切两段
        let p0 = &raw.pages[0];
        assert_eq!(p0.items.len(), 2);
        assert_eq!(p0.items[0].text, "风起于青萍之末。");
        assert_eq!(p0.items[1].text, "浪成于微澜之间。");

        // GBK 页：编码探测生效（encoding feature 下逐字还原；未开时 lossy
        // 兜底，段落结构（空行）仍在）
        let p2 = &raw.pages[2];
        assert_eq!(p2.items.len(), 2);
        #[cfg(feature = "encoding")]
        {
            assert_eq!(p2.items[0].text, "终章。");
            assert_eq!(p2.items[1].text, "全书完。");
        }

        // assemble：每段一个 Paragraph 块，页名透传
        let doc = ArchivePlugin.assemble(&raw);
        assert_eq!(doc.pages.len(), 3);
        assert_eq!(doc.pages[0].blocks.len(), 2);
        assert!(doc
            .pages[0]
            .blocks
            .iter()
            .all(|b| b.kind == BlockKind::Paragraph));
        assert_eq!(doc.pages[0].name.as_deref(), Some("第1章.txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 内层 zip 只递归一层：外层(1) → 内层 zip(2) 展开；第三层 zip 不再展开，
    /// 记深度上限 warning。
    #[test]
    fn nested_zip_depth_limited() {
        let dir = tmpdir("nested");
        let deep = build_zip(&dir, "deep.zip", &[("deep.txt", b"deepest".to_vec())]);
        let deep_bytes = std::fs::read(&deep).unwrap();
        let mid = build_zip(
            &dir,
            "mid.zip",
            &[("deep.zip", deep_bytes), ("mid.txt", b"middle".to_vec())],
        );
        let mid_bytes = std::fs::read(&mid).unwrap();
        let outer = build_zip(
            &dir,
            "outer.zip",
            &[("mid.zip", mid_bytes), ("outer.txt", b"outer".to_vec())],
        );

        let raw = ArchiveRawBackend.parse(outer.to_str().unwrap()).unwrap();
        // 外层文本 + 内层 zip（第 2 层）文本；第 3 层 deep.zip 不展开
        assert_eq!(page_names(&raw), vec!["mid.zip/mid.txt", "outer.txt"]);
        assert!(!page_names(&raw).iter().any(|n| n.contains("deep.txt")));
        let w = raw.meta.get("warnings").expect("应有深度告警");
        assert!(w.contains("深度上限"), "{w}");
        assert!(w.contains("deep.zip"), "{w}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 内层二进制文档格式记 warning（与文本页共存）；图片条目不告警。
    #[test]
    fn binary_inside_warns() {
        let dir = tmpdir("binary");
        let p = build_zip(
            &dir,
            "mixed.zip",
            &[
                ("a.txt", "正文一段".as_bytes().to_vec()),
                ("book.pdf", b"%PDF-1.4\nfake".to_vec()),
                ("cover.jpg", vec![0xFF, 0xD8, 0xFF, 0xE0]),
            ],
        );
        let raw = ArchiveRawBackend.parse(p.to_str().unwrap()).unwrap();
        assert_eq!(page_names(&raw), vec!["a.txt"]);
        let w = raw.meta.get("warnings").expect("应有二进制格式告警");
        assert!(w.contains("pdf"), "{w}");
        assert!(w.contains("解包后单独解析"), "{w}");
        assert!(!w.contains("jpg"), "图片条目不应告警：{w}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rar：走纯 Rust rars 库路线（选型核实结论——rars 0.12.3 纯 Rust、
    /// 无 libunrar；外部命令 unrar/7z 路线不启用）。真 RAR5 fixture 解出
    /// 两页中文 txt。
    #[test]
    fn rar_external_command_or_lib() {
        let dir = tmpdir("rar");
        let p = dir.join("book.rar");
        std::fs::write(&p, hex_decode(RAR_FIXTURE_HEX)).unwrap();
        let raw = ArchiveRawBackend.parse(p.to_str().unwrap()).unwrap();
        assert_eq!(page_names(&raw), vec!["第一章.txt", "第二章.txt"]);
        let p0 = &raw.pages[0];
        assert_eq!(p0.items.len(), 2);
        assert_eq!(p0.items[0].text, "风起于青萍之末。");
        assert_eq!(p0.items[1].text, "浪成于微澜之间。");
        assert_eq!(raw.pages[1].items[0].text, "少年推门而出，天地一白。");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 7z：sevenz-rust2 库路线（临时目录解压即用即删）。真 7z fixture 解出
    /// 两页中文 txt。
    #[test]
    fn sevenz_lib_extracts_txt() {
        let dir = tmpdir("sevenz");
        let p = dir.join("book.7z");
        std::fs::write(&p, hex_decode(SEVENZ_FIXTURE_HEX)).unwrap();
        let raw = ArchiveRawBackend.parse(p.to_str().unwrap()).unwrap();
        assert_eq!(page_names(&raw), vec!["第一章.txt", "第二章.txt"]);
        assert_eq!(raw.pages[0].items[0].text, "风起于青萍之末。");
        // 临时目录已清理
        let leftovers: Vec<PathBuf> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("docforge-7z-"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(leftovers.is_empty(), "临时目录未清理：{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空压缩包（无任何条目）→ corrupt。
    #[test]
    fn empty_archive_is_corrupt() {
        let dir = tmpdir("empty");
        let p = build_zip(&dir, "empty.zip", &[]);
        let err = ArchiveRawBackend.parse(p.to_str().unwrap()).unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全图片条目 → corrupt + 疑似 CBZ 文案（router 分流说明的行为依据）。
    #[test]
    fn images_only_zip_suggests_cbz() {
        let dir = tmpdir("imgs");
        let p = build_zip(
            &dir,
            "pics.zip",
            &[("p1.jpg", vec![0xFF, 0xD8]), ("p2.png", vec![0x89, b'P'])],
        );
        let err = ArchiveRawBackend.parse(p.to_str().unwrap()).unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(err.message.contains("CBZ"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 仅二进制文档（无文本）→ corrupt，错误信息列出格式。
    #[test]
    fn binary_only_zip_is_corrupt() {
        let dir = tmpdir("binonly");
        let p = build_zip(
            &dir,
            "docs.zip",
            &[("a.pdf", b"%PDF-1.4".to_vec()), ("b.epub", b"PKxxxx".to_vec())],
        );
        let err = ArchiveRawBackend.parse(p.to_str().unwrap()).unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(err.message.contains("pdf"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ext_of / classify / 自然排序单元用例。
    #[test]
    fn classify_and_natural_order_unit() {
        assert_eq!(ext_of("a/b/第1章.TXT"), Some("txt".to_string()));
        assert_eq!(ext_of("noext"), None);
        assert_eq!(ext_of("dir.d/"), None);
        assert_eq!(classify("x/b.pdf"), EntryClass::Doc("pdf"));
        assert_eq!(classify("x/inner.ZIP"), EntryClass::InnerZip);
        assert_eq!(classify("x/enc.7z"), EntryClass::InnerOtherArchive);
        assert_eq!(classify("x/c.PNG"), EntryClass::Image);
        assert_eq!(classify("x/notes.txt"), EntryClass::Text);
        assert_eq!(classify("x/junk.dat"), EntryClass::Other);
        assert_eq!(natural_cmp("第2章.txt", "第10章.txt"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("a/b2.txt", "a/b10.txt"), std::cmp::Ordering::Less);
    }
}
