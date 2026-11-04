//! 解析吞吐/内存基准（harness = false，自带 main；数字与方法学见 BENCHMARK.md）。
//!
//! 设计要点：
//! - fixture 程序合成：确定性模式块循环（无随机源、无外部 fixture），跑时流式写入
//!   系统临时目录、结束整体清理。生成器只持有当前模式块（KB 级），fixture 构造
//!   不抬高被测峰值内存；
//! - 指标：每 case 预热 1 轮 + 计时 3 轮取中位；吞吐 = 输入字节 / 中位耗时；
//! - 峰值内存读 `/proc/self/status` 的 `VmHWM`——进程生命期高点、不可重置：
//!   多 case 同进程跑只能报"累计高点"（默认模式只在表尾注明）；逐 case 真实峰值
//!   用 `--single` 模式（单进程单 case，外层脚本逐 case 调用，用法见 `--help`）；
//! - docx / epub / xlsx fixture 为手写最小合法容器（STORED 无压缩 zip 条目 + CRC32），
//!   不依赖 docforge 的可选 zip 依赖——bench 在对应 feature 关闭时不该编译失败；
//!   容器内容用两遍流式生成（第一遍算长/CRC、第二遍写盘），模式生成是纯函数，
//!   两遍字节一致。
//! - 回写族（wb-\*）：回写单程吞吐——setup（不计入计时）parse → to_translation_units
//!   → 确定性假译文（`译:{原文}`，`(id, 译)` 二元组形态、信任锚点），计时只含
//!   `writeback_*` 一次调用（内部按对齐契约重解析原文件，该成本计入——与真实
//!   翻译管线一致）。wb-docx / wb-epub / wb-xlsx fixture 各含 1MB 确定性伪二进制
//!   条目（锻炼 raw_copy 直拷：未触碰条目不解压不重压）；written 列 = 实际写入
//!   单元数，≠ 送译数或有 skipped 直接判错（防静默零写入的假吞吐）。
//!   wb-docx-noread = 同 fixture 只 parse 的对照行，量化回写增量成本——基线服务
//!   "流式 apply_edits" 改造的前后对照（改造前拼接产物整份驻留，VmHWM 含该峰值）。
//!
//! 流式对比（txt-stream / md-stream）：parse vs parse_streaming 计时对比。
//! 单页文本件流式无内存差异——如实记录；流式的内存收益场景在 pdf 千页件
//!（页产即吐、峰值 = 单页），本机无 pdftoppm 渲染链与 OCR 模型，不测、不编数。

#![allow(clippy::type_complexity)] // 合成 fixture 的表驱动签名（存量取舍）
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

// ── 常量 ─────────────────────────────────────────────────────────────

/// fixture 目标大小（字节）：~5MB / ~2MB 的固定锚点（实际字节数以生成后文件为准）
const TARGET_MD: usize = 5 * 1024 * 1024;
const TARGET_HTML: usize = 5 * 1024 * 1024;
const TARGET_TXT: usize = 5 * 1024 * 1024;
const TARGET_CSV: usize = 2 * 1024 * 1024;
/// docx 的 word/document.xml 正文目标大小
#[cfg(feature = "docx")]
const TARGET_DOCX_XML: usize = 2 * 1024 * 1024;
/// epub 单章目标大小（章节数 × 单章 ≈ 2MB 容器）
#[cfg(feature = "epub")]
const TARGET_EPUB_CHAPTER: usize = 64 * 1024;
#[cfg(feature = "epub")]
const EPUB_CHAPTERS: usize = 32;
/// wb-* fixture 的伪二进制条目（伪图片）大小：1MB——锻炼 raw_copy 直拷路径
/// （未触碰条目不解压不重压；内容无需真图，确定性字节模式即可）
#[cfg(any(feature = "docx", feature = "epub", feature = "xlsx"))]
const TARGET_WB_BINARY: usize = 1024 * 1024;
/// wb-xlsx 的 sheet XML 正文目标大小
#[cfg(feature = "xlsx")]
const TARGET_XLSX_SHEET: usize = 2 * 1024 * 1024;
/// wb-xlsx 每行列数（A–D 列）
#[cfg(feature = "xlsx")]
const XLSX_COLS: usize = 4;

/// 轮次规则：预热 + 计时取中位
const WARMUP_ROUNDS: usize = 1;
const TIMED_ROUNDS: usize = 3;

/// 模式段正文：混合中英文与标点，模拟真实段落密度。
/// 不含需转义的 XML / Markdown 特殊字符（尖括号、与符号、反引号等由各格式模板控制）。
const PARA: &str = "本段为基准测试合成正文：docforge 把统一块级 IR、格式路由后端与流式消费组合起来，将源文档解析为页级块结构，供解析转换、OCR 与翻译管线消费。The quick brown fox jumps over the lazy dog while the engine streams parsed pages to the consumer. ";

// ── case 表 ──────────────────────────────────────────────────────────

/// case 形态：解析族（Parse / Stream）与回写族（Writeback / WbBaseline）
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 全量 parse
    Parse,
    /// parse_streaming（流式消费，sink 即收即弃）
    Stream,
    /// 回写单程（BilingualInline）：setup 建假译文对（不计入计时），计时只含
    /// writeback_* 一次调用（内部重解析计入）
    Writeback(WbTarget),
    /// 对照行：同 fixture 只 parse 不回写（计时口径与解析族同），量化回写增量
    WbBaseline,
}

/// 回写目标格式（txt 恒可用；docx / epub / xlsx 随对应 feature）
#[derive(Clone, Copy, PartialEq, Eq)]
enum WbTarget {
    Txt,
    #[cfg(feature = "docx")]
    Docx,
    #[cfg(feature = "epub")]
    Epub,
    #[cfg(feature = "xlsx")]
    Xlsx,
}

struct Case {
    name: &'static str,
    /// 输入 fixture 文件名（临时目录内；多 case 可复用同一文件）
    file: &'static str,
    kind: Kind,
}

/// 当前编译面可用的全部 case（txt 恒可用；md/csv/html 随 default feature；
/// docx/epub/xlsx 随对应 feature；wb-\* = 回写族，fixture 见 ensure_fixture）。
/// case 顺序 = 输出表行序（解析族在前、回写族在后，两张表分开打印）。
fn all_cases() -> Vec<Case> {
    let mut cases = vec![Case { name: "txt", file: "bench_txt.txt", kind: Kind::Parse }];
    #[cfg(feature = "md")]
    cases.push(Case { name: "md", file: "bench_md.md", kind: Kind::Parse });
    #[cfg(feature = "html")]
    cases.push(Case { name: "html", file: "bench_html.html", kind: Kind::Parse });
    #[cfg(feature = "csv")]
    cases.push(Case { name: "csv", file: "bench_csv.csv", kind: Kind::Parse });
    cases.push(Case { name: "txt-stream", file: "bench_txt.txt", kind: Kind::Stream });
    #[cfg(feature = "md")]
    cases.push(Case { name: "md-stream", file: "bench_md.md", kind: Kind::Stream });
    #[cfg(feature = "docx")]
    cases.push(Case { name: "docx", file: "bench_docx.docx", kind: Kind::Parse });
    #[cfg(feature = "epub")]
    cases.push(Case { name: "epub", file: "bench_epub.epub", kind: Kind::Parse });
    // ── 回写族（wb-*）：吞吐 = 输入字节 / 回写单程中位耗时，见文件头与 BENCHMARK.md ──
    cases.push(Case {
        name: "wb-txt",
        file: "bench_txt.txt",
        kind: Kind::Writeback(WbTarget::Txt),
    });
    #[cfg(feature = "docx")]
    cases.push(Case {
        name: "wb-docx",
        file: "bench_wb_docx.docx",
        kind: Kind::Writeback(WbTarget::Docx),
    });
    #[cfg(feature = "docx")]
    cases.push(Case {
        name: "wb-docx-noread",
        file: "bench_wb_docx.docx",
        kind: Kind::WbBaseline,
    });
    #[cfg(feature = "epub")]
    cases.push(Case {
        name: "wb-epub",
        file: "bench_wb_epub.epub",
        kind: Kind::Writeback(WbTarget::Epub),
    });
    #[cfg(feature = "xlsx")]
    cases.push(Case {
        name: "wb-xlsx",
        file: "bench_wb_xlsx.xlsx",
        kind: Kind::Writeback(WbTarget::Xlsx),
    });
    cases
}

// ── fixture 模式块（纯函数：内容只依赖序号，两遍生成字节一致）────────

fn txt_unit(i: usize) -> String {
    format!("第 {i} 段：{PARA}{PARA}\n\n")
}

/// 标题 / 段落 / 列表 / 表格 / 代码块固定模式循环
fn md_unit(i: usize) -> String {
    format!(
        "## 第 {i} 小节：合成基准数据\n\n{PARA}{PARA}\n\
         - 列表项甲：块级 IR 统一段落、标题、表格与代码块语义\n\
         - 列表项乙：格式路由按扩展名与 magic 嗅探后端\n\
         - 列表项丙：流式消费逐页投递，大件边产边耗\n\n\
         | 指标 | 数值 | 说明 |\n\
         | --- | --- | --- |\n\
         | 吞吐 | {i} MB/s | 合成数据 |\n\
         | 延迟 | {i} ms | 中位数 |\n\n\
         ```rust\nfn sample_{i}() -> u32 {{ {i} * 2 }}\n```\n\n"
    )
}

/// h2 / p / table 固定模式循环
fn html_unit(i: usize) -> String {
    format!(
        "<h2>第 {i} 小节：合成基准数据</h2>\n<p>{PARA}{PARA}</p>\n\
         <table>\n<tr><th>指标</th><th>数值</th></tr>\n\
         <tr><td>吞吐</td><td>{i}</td></tr>\n\
         <tr><td>延迟</td><td>{i} ms</td></tr>\n</table>\n"
    )
}

/// 数字表格行（确定性整数运算，分计价、无浮点无随机）；i == 0 时带表头
fn csv_unit(i: usize) -> String {
    let head = if i == 0 { "序号,名称,数量,单价,金额,备注\n" } else { "" };
    let qty = i % 97;
    let cents = (i % 9973) + 100; // 单价（分）
    let amount = qty * cents; // 金额（分）
    format!("{head}{i},样本件-{i},{qty},{},{:02},{},{:02},基准合成数据行\n",
        cents / 100, cents % 100, amount / 100, amount % 100)
}

// ── docx / epub / xlsx：手写最小合法容器（STORED zip）─────────────────

#[cfg(feature = "docx")]
const DOCX_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body>";
#[cfg(feature = "docx")]
const DOCX_TAIL: &str = "</w:body></w:document>";

/// docx 正文单元：每 16 个正文段插一个 Heading2 样式段——给 auto 结构探针
/// 提供结构信号（与真实文档一致，走 semantic 组装出块）
#[cfg(feature = "docx")]
fn docx_unit(i: usize) -> String {
    if i.is_multiple_of(16) {
        format!(
            "<w:p><w:pPr><w:pStyle w:val=\"Heading2\"/></w:pPr>\
             <w:r><w:t>第 {i} 小节标题：合成基准数据</w:t></w:r></w:p>"
        )
    } else {
        format!("<w:p><w:r><w:t>第 {i} 段：{PARA}</w:t></w:r></w:p>")
    }
}

#[cfg(feature = "epub")]
const EPUB_CONTAINER: &str = "<?xml version=\"1.0\"?>\n\
<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\n  \
<rootfiles><rootfile full-path=\"OEBPS/content.opf\" media-type=\"application/oebps-package+xml\"/></rootfiles>\n\
</container>";

#[cfg(feature = "epub")]
fn epub_opf() -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" \
         version=\"3.0\" unique-identifier=\"id\">\n<metadata \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>合成基准书</dc:title>\
         <dc:language>zh</dc:language><dc:identifier id=\"id\">urn:uuid:docforge-bench</dc:identifier>\
         </metadata>\n<manifest>\n",
    );
    s.push_str("<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n");
    for n in 1..=EPUB_CHAPTERS {
        s.push_str(&format!(
            "<item id=\"ch{n}\" href=\"text/ch{n}.xhtml\" media-type=\"application/xhtml+xml\"/>\n"
        ));
    }
    s.push_str("</manifest>\n<spine>\n");
    for n in 1..=EPUB_CHAPTERS {
        s.push_str(&format!("<itemref idref=\"ch{n}\"/>\n"));
    }
    s.push_str("</spine>\n</package>");
    s
}

#[cfg(feature = "epub")]
fn epub_nav() -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><body>\n\
         <nav epub:type=\"toc\" xmlns:epub=\"http://www.idpf.org/2007/ops\"><ol>\n",
    );
    for n in 1..=EPUB_CHAPTERS {
        s.push_str(&format!("<li><a href=\"text/ch{n}.xhtml\">第 {n} 章</a></li>\n"));
    }
    s.push_str("</ol></nav></body></html>");
    s
}

// ── 条目内容描述 + 手写 STORED zip ──────────────────────────────────

/// 条目内容：静态串 or 确定性流（两遍迭代产同样字节——双遍写盘依赖）
#[cfg(any(feature = "docx", feature = "epub", feature = "xlsx"))]
enum Body<'a> {
    Owned(String),
    Stream(&'a dyn Fn(&mut dyn FnMut(&[u8]))),
}

#[cfg(any(feature = "docx", feature = "epub", feature = "xlsx"))]
struct EntryDesc<'a> {
    name: String,
    body: Body<'a>,
}

#[cfg(any(feature = "docx", feature = "epub", feature = "xlsx"))]
fn stream_body(body: &Body<'_>, emit: &mut dyn FnMut(&[u8])) {
    match body {
        Body::Owned(s) => emit(s.as_bytes()),
        Body::Stream(f) => (*f)(emit),
    }
}

/// 模式块循环流：反复调 make(序号, buf) 直到累计 ≥ target
fn loop_until(target: usize, make: &dyn Fn(usize, &mut Vec<u8>), emit: &mut dyn FnMut(&[u8])) {
    let mut buf = Vec::with_capacity(16 * 1024);
    let mut total = 0usize;
    let mut i = 0usize;
    while total < target {
        buf.clear();
        make(i, &mut buf);
        total += buf.len();
        emit(&buf);
        i += 1;
    }
}

/// CRC-32（IEEE，多项式 0xEDB88320）查表实现：仅用于 fixture zip 条目校验
#[cfg(any(feature = "docx", feature = "epub", feature = "xlsx"))]
fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    });
    let mut c = !crc;
    for &b in data {
        c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// 手写最小 zip（STORED 无压缩条目）：bench 目标在 docx/epub feature 关闭时
/// 没有 zip 依赖可用，且 STORED 使生成零压缩成本。固定 mod 时间保字节确定性。
/// 双遍流水：第一遍流式算每条目长/CRC（不驻留），第二遍写盘 + 中央目录 + EOCD。
#[cfg(any(feature = "docx", feature = "epub", feature = "xlsx"))]
fn write_stored_zip(path: &Path, entries: &[EntryDesc<'_>]) -> std::io::Result<u64> {
    // pass 1：尺寸 + CRC
    let mut metas: Vec<(u64, u32)> = Vec::with_capacity(entries.len());
    for e in entries {
        let mut len = 0u64;
        let mut crc = 0u32;
        stream_body(&e.body, &mut |chunk: &[u8]| {
            len += chunk.len() as u64;
            crc = crc32_update(crc, chunk);
        });
        metas.push((len, crc));
    }
    // 本地头偏移
    let mut offsets = Vec::with_capacity(entries.len());
    let mut off = 0u64;
    for (e, (len, _)) in entries.iter().zip(&metas) {
        offsets.push(off);
        off += 30 + e.name.len() as u64 + len;
    }

    let file = fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(64 * 1024, file);
    const DATE: u16 = 0x0021; // 1980-01-01（固定值保确定性）
    // pass 2：本地文件头 + 条目数据
    for (idx, e) in entries.iter().enumerate() {
        let (len, crc) = metas[idx];
        let name = e.name.as_bytes();
        w.write_all(&0x0403_4b50u32.to_le_bytes())?; // local file header sig
        w.write_all(&20u16.to_le_bytes())?; // version needed
        w.write_all(&0u16.to_le_bytes())?; // flags
        w.write_all(&0u16.to_le_bytes())?; // method = stored
        w.write_all(&0u16.to_le_bytes())?; // mod time
        w.write_all(&DATE.to_le_bytes())?;
        w.write_all(&crc.to_le_bytes())?;
        w.write_all(&(len as u32).to_le_bytes())?; // compressed size
        w.write_all(&(len as u32).to_le_bytes())?; // uncompressed size
        w.write_all(&(name.len() as u16).to_le_bytes())?;
        w.write_all(&0u16.to_le_bytes())?; // extra len
        w.write_all(name)?;
        stream_body(&e.body, &mut |chunk: &[u8]| {
            let _ = w.write_all(chunk);
        });
    }
    // 中央目录
    let cd_start = off;
    let mut cd: Vec<u8> = Vec::with_capacity(entries.len() * 64);
    for (idx, e) in entries.iter().enumerate() {
        let (len, crc) = metas[idx];
        let name = e.name.as_bytes();
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central dir sig
        cd.extend_from_slice(&20u16.to_le_bytes()); // version made by
        cd.extend_from_slice(&20u16.to_le_bytes()); // version needed
        cd.extend_from_slice(&0u16.to_le_bytes()); // flags
        cd.extend_from_slice(&0u16.to_le_bytes()); // method
        cd.extend_from_slice(&0u16.to_le_bytes()); // mod time
        cd.extend_from_slice(&DATE.to_le_bytes());
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&(len as u32).to_le_bytes()); // compressed size
        cd.extend_from_slice(&(len as u32).to_le_bytes()); // uncompressed size
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes()); // extra len
        cd.extend_from_slice(&0u16.to_le_bytes()); // comment len
        cd.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        cd.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        cd.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        cd.extend_from_slice(&(offsets[idx] as u32).to_le_bytes());
        cd.extend_from_slice(name);
    }
    w.write_all(&cd)?;
    // EOCD
    w.write_all(&0x0605_4b50u32.to_le_bytes())?;
    w.write_all(&0u16.to_le_bytes())?; // disk number
    w.write_all(&0u16.to_le_bytes())?; // disk with central dir
    w.write_all(&(entries.len() as u16).to_le_bytes())?;
    w.write_all(&(entries.len() as u16).to_le_bytes())?;
    w.write_all(&(cd.len() as u32).to_le_bytes())?;
    w.write_all(&(cd_start as u32).to_le_bytes())?;
    w.write_all(&0u16.to_le_bytes())?; // comment len
    w.flush()?;
    Ok(fs::metadata(path)?.len())
}

/// 伪二进制条目（伪图片）流：确定性字节模式（全局序号 % 251），64KB 分块发射
/// ——生成器只驻留一个块。raw_copy 只直拷压缩流，内容无需真图；闭包为纯函数，
/// 两遍生成字节一致（双遍写盘依赖）。
#[cfg(any(feature = "docx", feature = "epub", feature = "xlsx"))]
fn binary_body() -> &'static dyn Fn(&mut dyn FnMut(&[u8])) {
    // Box::leak 得 'static 引用（&'static 闭包字面量不会被常量提升）
    Box::leak(Box::new(move |emit: &mut dyn FnMut(&[u8])| {
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0usize;
        while done < TARGET_WB_BINARY {
            let n = buf.len().min(TARGET_WB_BINARY - done);
            for (j, b) in buf.iter_mut().enumerate().take(n) {
                *b = ((done + j) % 251) as u8;
            }
            emit(&buf[..n]);
            done += n;
        }
    }))
}

/// docx fixture：word/document.xml（引擎 smoke 同款最小骨架）；`with_image` =
/// 追加 1MB 伪图片条目（wb-* 锻炼 raw_copy 直拷路径；解析面 image 条目被忽略）
#[cfg(feature = "docx")]
fn docx_entries(with_image: bool) -> Vec<EntryDesc<'static>> {
    // Box::leak 得 'static 引用（&'static 闭包字面量不会被常量提升）
    let f: &'static dyn Fn(&mut dyn FnMut(&[u8])) = Box::leak(Box::new(
        move |emit: &mut dyn FnMut(&[u8])| {
            emit(DOCX_HEAD.as_bytes());
            loop_until(TARGET_DOCX_XML, &|i, buf| {
                buf.extend_from_slice(docx_unit(i).as_bytes());
            }, emit);
            emit(DOCX_TAIL.as_bytes());
        },
    ));
    let mut entries =
        vec![EntryDesc { name: "word/document.xml".to_string(), body: Body::Stream(f) }];
    if with_image {
        entries.push(EntryDesc {
            name: "word/media/image1.bin".to_string(),
            body: Body::Stream(binary_body()),
        });
    }
    entries
}

/// epub fixture：mimetype + container + OPF + nav + 32 章（一章一页，spine 序 =
/// 页序）；`with_image` = 追加 1MB 伪图片条目（wb-* 锻炼 raw_copy 直拷路径；
/// 未进 manifest/spine，解析面忽略）
#[cfg(feature = "epub")]
fn epub_entries(with_image: bool) -> Vec<EntryDesc<'static>> {
    let mut entries = vec![
        EntryDesc {
            name: "mimetype".to_string(),
            body: Body::Owned("application/epub+zip".to_string()),
        },
        EntryDesc {
            name: "META-INF/container.xml".to_string(),
            body: Body::Owned(EPUB_CONTAINER.to_string()),
        },
        EntryDesc { name: "OEBPS/content.opf".to_string(), body: Body::Owned(epub_opf()) },
        EntryDesc { name: "OEBPS/nav.xhtml".to_string(), body: Body::Owned(epub_nav()) },
    ];
    for n in 1..=EPUB_CHAPTERS {
        // Box::leak 得 'static 引用（&'static 闭包字面量不会被常量提升）
        let f: &'static dyn Fn(&mut dyn FnMut(&[u8])) = Box::leak(Box::new(
            move |emit: &mut dyn FnMut(&[u8])| {
                let head = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                     <html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>第 {n} 章</title></head>\
                     <body><h1>第 {n} 章：合成基准数据</h1>"
                );
                emit(head.as_bytes());
                loop_until(TARGET_EPUB_CHAPTER, &|i, buf| {
                    buf.extend_from_slice(
                        format!("<p>第 {n} 章 · 第 {i} 段：{PARA}</p>").as_bytes(),
                    );
                }, emit);
                emit(b"</body></html>");
            },
        ));
        entries.push(EntryDesc {
            name: format!("OEBPS/text/ch{n}.xhtml"),
            body: Body::Stream(f),
        });
    }
    if with_image {
        entries.push(EntryDesc {
            name: "OEBPS/media/image1.bin".to_string(),
            body: Body::Stream(binary_body()),
        });
    }
    entries
}

// ── xlsx：最小合法包（workbook + rels + inlineStr sheet）─────────────

#[cfg(feature = "xlsx")]
const XLSX_WORKBOOK: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
<sheets><sheet name=\"S1\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>";

#[cfg(feature = "xlsx")]
const XLSX_WB_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" \
Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" \
Target=\"worksheets/sheet1.xml\"/></Relationships>";

/// 列号 → A1 列字母（4 列只到 D；写成通用形态防未来调列数）
#[cfg(feature = "xlsx")]
fn xlsx_col_ref(c: usize) -> String {
    let mut n = c + 1;
    let mut s = String::new();
    while n > 0 {
        n -= 1;
        s.insert(0, (b'A' + (n % 26) as u8) as char);
        n /= 26;
    }
    s
}

/// xlsx 行单元：XLSX_COLS 个 inlineStr 字符格（文本 = 行列号 + PARA——PARA 不含
/// XML 特殊字符免转义；全字符格 = 无类型守卫跳过，全非空 = 格位 ↔ 格单元 1:1
/// 对位；确定性——两遍生成字节一致）
#[cfg(feature = "xlsx")]
fn xlsx_row(i: usize, buf: &mut Vec<u8>) {
    let mut s = format!("<row r=\"{}\">", i + 1);
    for c in 0..XLSX_COLS {
        s.push_str(&format!(
            "<c r=\"{col}{row}\" t=\"inlineStr\"><is><t>表 {row} 行 {col} 列：{PARA}</t></is></c>",
            col = xlsx_col_ref(c),
            row = i + 1,
        ));
    }
    s.push_str("</row>");
    buf.extend_from_slice(s.as_bytes());
}

/// xlsx fixture：最小合法包（workbook.xml + workbook rels + 单 sheet，无 sst /
/// styles——两侧解析对缺失条目均优雅回退）+ 1MB 伪二进制条目（wb-* 锻炼
/// raw_copy 直拷路径）。格全为 inlineStr 字符格，回写改写为 inlineStr 同型。
#[cfg(feature = "xlsx")]
fn xlsx_entries() -> Vec<EntryDesc<'static>> {
    let sheet: &'static dyn Fn(&mut dyn FnMut(&[u8])) = Box::leak(Box::new(
        move |emit: &mut dyn FnMut(&[u8])| {
            emit(br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#);
            loop_until(TARGET_XLSX_SHEET, &|i, buf| xlsx_row(i, buf), emit);
            emit(b"</sheetData></worksheet>");
        },
    ));
    vec![
        EntryDesc {
            name: "xl/workbook.xml".to_string(),
            body: Body::Owned(XLSX_WORKBOOK.to_string()),
        },
        EntryDesc {
            name: "xl/_rels/workbook.xml.rels".to_string(),
            body: Body::Owned(XLSX_WB_RELS.to_string()),
        },
        EntryDesc { name: "xl/worksheets/sheet1.xml".to_string(), body: Body::Stream(sheet) },
        EntryDesc { name: "xl/media/blob1.bin".to_string(), body: Body::Stream(binary_body()) },
    ]
}

// ── fixture 落盘 ─────────────────────────────────────────────────────

/// 流式写文件（生成器只持有当前模式块，不整体驻留），返回实际字节数
fn write_stream_file(
    path: &Path,
    stream: &dyn Fn(&mut dyn FnMut(&[u8])),
) -> std::io::Result<u64> {
    let f = fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(64 * 1024, f);
    let mut total = 0u64;
    let mut err: Option<std::io::Error> = None;
    stream(&mut |chunk: &[u8]| {
        if err.is_some() {
            return;
        }
        total += chunk.len() as u64;
        if let Err(e) = w.write_all(chunk) {
            err = Some(e);
        }
    });
    w.flush()?;
    match err {
        Some(e) => Err(e),
        None => Ok(total),
    }
}

/// 生成 case 依赖的 fixture（已存在则跳过——多 case 复用同一文件）
fn ensure_fixture(dir: &Path, file: &str) -> std::io::Result<u64> {
    let path = dir.join(file);
    if let Ok(meta) = fs::metadata(&path) {
        return Ok(meta.len());
    }
    match file {
        "bench_txt.txt" => write_stream_file(&path, &|emit| {
            loop_until(TARGET_TXT, &|i, buf| buf.extend_from_slice(txt_unit(i).as_bytes()), emit)
        }),
        "bench_md.md" => write_stream_file(&path, &|emit| {
            loop_until(TARGET_MD, &|i, buf| buf.extend_from_slice(md_unit(i).as_bytes()), emit)
        }),
        "bench_html.html" => write_stream_file(&path, &|emit| {
            loop_until(TARGET_HTML, &|i, buf| buf.extend_from_slice(html_unit(i).as_bytes()), emit)
        }),
        "bench_csv.csv" => write_stream_file(&path, &|emit| {
            loop_until(TARGET_CSV, &|i, buf| buf.extend_from_slice(csv_unit(i).as_bytes()), emit)
        }),
        #[cfg(feature = "docx")]
        "bench_docx.docx" => {
            let entries = docx_entries(false);
            write_stored_zip(&path, &entries)
        }
        #[cfg(feature = "epub")]
        "bench_epub.epub" => {
            let entries = epub_entries(false);
            write_stored_zip(&path, &entries)
        }
        #[cfg(feature = "docx")]
        "bench_wb_docx.docx" => {
            let entries = docx_entries(true);
            write_stored_zip(&path, &entries)
        }
        #[cfg(feature = "epub")]
        "bench_wb_epub.epub" => {
            let entries = epub_entries(true);
            write_stored_zip(&path, &entries)
        }
        #[cfg(feature = "xlsx")]
        "bench_wb_xlsx.xlsx" => {
            let entries = xlsx_entries();
            write_stored_zip(&path, &entries)
        }
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("未知 fixture: {other}"),
        )),
    }
}

// ── 计时 ─────────────────────────────────────────────────────────────

struct Row {
    name: &'static str,
    bytes: u64,
    median: Duration,
    /// 回写族：实际写入单元数（WritebackReport.written；防静默零写入的假吞吐）；
    /// 解析族 None
    written: Option<usize>,
    /// true = 回写族行（进回写表；wb-docx-noread 对照行也在回写表）
    wb: bool,
}

/// 全量 parse 一轮：返回页数（校验 fixture/路由有效，兼防结果被优化掉）
fn parse_once(path: &str) -> Result<usize, String> {
    let doc = docforge::parse(path).map_err(|e| format!("{}（code={:?}）", e.message, e.code))?;
    Ok(doc.pages.len())
}

/// 流式 parse 一轮：sink 即收即弃（只数页），返回页数
fn stream_once(path: &str) -> Result<usize, String> {
    let mut pages = 0usize;
    let meta = docforge::parse_streaming(path, &mut |_page: docforge::Page| {
        pages += 1;
        true
    })
    .map_err(|e| format!("{}（code={:?}）", e.message, e.code))?;
    if meta.page_count != pages {
        return Err(format!(
            "parse_streaming meta.page_count({}) != sink 收到页数({pages})",
            meta.page_count
        ));
    }
    Ok(pages)
}

// ── 回写族（wb-*）────────────────────────────────────────────────────

/// 回写 setup（不计入计时）：parse → to_translation_units → 确定性假译文对
/// （`译:{原文}`；`(id, 译文)` 二元组形态 = `source: None` 信任锚点——与
/// `TranslationPair` 的 From 二元组一致）。返回 (译文对, 对数)。
fn wb_pairs(path: &str) -> Result<(Vec<docforge::emit::TranslationPair>, usize), String> {
    let doc = docforge::parse(path).map_err(|e| format!("{}（code={:?}）", e.message, e.code))?;
    let units = docforge::emit::to_translation_units(&doc);
    if units.is_empty() {
        return Err("回写 setup：parse 产出 0 个翻译单元——fixture 或路由异常".to_string());
    }
    let pairs: Vec<docforge::emit::TranslationPair> = units
        .iter()
        .map(|u| (u.id.clone(), format!("译:{}", u.text)).into())
        .collect();
    let n = pairs.len();
    Ok((pairs, n))
}

/// 回写单程一次调用（BilingualInline）。签名契约：四个 writeback_* 均为
/// `(in_path: impl AsRef<Path>, pairs: &[TranslationPair], strategy, out: impl AsRef<Path>)
/// -> Result<WritebackReport>`（与 src/writeback 现状逐字比对过）。
fn wb_once(
    target: WbTarget,
    in_path: &Path,
    pairs: &[docforge::emit::TranslationPair],
    out_path: &Path,
) -> Result<docforge::writeback::WritebackReport, String> {
    let call = match target {
        WbTarget::Txt => docforge::writeback::writeback_txt(
            in_path,
            pairs,
            docforge::emit::BiStrategy::BilingualInline,
            out_path,
        ),
        #[cfg(feature = "docx")]
        WbTarget::Docx => docforge::writeback::writeback_docx(
            in_path,
            pairs,
            docforge::emit::BiStrategy::BilingualInline,
            out_path,
        ),
        #[cfg(feature = "epub")]
        WbTarget::Epub => docforge::writeback::writeback_epub(
            in_path,
            pairs,
            docforge::emit::BiStrategy::BilingualInline,
            out_path,
        ),
        #[cfg(feature = "xlsx")]
        WbTarget::Xlsx => docforge::writeback::writeback_xlsx(
            in_path,
            pairs,
            docforge::emit::BiStrategy::BilingualInline,
            out_path,
        ),
    };
    call.map_err(|e| format!("{}（code={:?}）", e.message, e.code))
}

/// 跑回写 case：setup（parse→单元→假译文对，不计入计时）+ 预热 1 轮 + 计时
/// 3 轮取中位（每轮 = writeback_* 一次调用，原子落盘到临时目录内 out 文件）。
/// 守卫：确定性 fixture 下每个假译文都应写入——`written == 送译数` 且
/// `skipped` 为空，`written == 0` / 有跳过 = fixture 或锚点失配，判错拒报
/// 吞吐（防静默零写入的假吞吐）。
fn run_wb_case(
    case: &Case,
    dir: &Path,
    path_str: &str,
    bytes: u64,
    target: WbTarget,
) -> Result<(Row, usize), String> {
    let (pairs, n_pairs) = wb_pairs(path_str)?;
    let out_path = dir.join(format!("out_{}", case.name));
    let run = |round: &str| -> Result<usize, String> {
        let report = wb_once(target, Path::new(path_str), &pairs, &out_path)?;
        if report.written == 0 {
            return Err(format!(
                "{}：{round} 轮 written=0——fixture 或锚点失配，拒报吞吐",
                case.name
            ));
        }
        if report.written != n_pairs || !report.skipped.is_empty() {
            let heads: Vec<String> = report
                .skipped
                .iter()
                .take(5)
                .map(|(id, why)| format!("{id}: {why}"))
                .collect();
            return Err(format!(
                "{}：{round} 轮 written {} ≠ 送译 {n_pairs} 或有 skipped（{heads:?}）\
                 ——确定性 fixture 下应全量写入",
                case.name, report.written
            ));
        }
        Ok(report.written)
    };
    for _ in 0..WARMUP_ROUNDS {
        run("预热")?;
    }
    let mut durs: Vec<Duration> = Vec::with_capacity(TIMED_ROUNDS);
    let mut written = 0usize;
    for _ in 0..TIMED_ROUNDS {
        let t = Instant::now();
        written = run("计时")?;
        durs.push(t.elapsed());
    }
    durs.sort();
    Ok((
        Row {
            name: case.name,
            bytes,
            median: durs[durs.len() / 2],
            written: Some(written),
            wb: true,
        },
        written,
    ))
}

/// 跑单个 case：预热 1 轮 + 计时 3 轮取中位。同时返回进度数字——解析族 =
/// 页数（fixture 有效性凭据：docx 走 semantic 出块、epub 32 章各一页），
/// 回写族 = written（实际写入单元数）。
fn run_case(case: &Case, dir: &Path) -> Result<(Row, usize), String> {
    let path = dir.join(case.file);
    let path_str = path.to_string_lossy().into_owned();
    let bytes = fs::metadata(&path).map_err(|e| format!("stat {path_str}: {e}"))?.len();

    match case.kind {
        Kind::Writeback(target) => run_wb_case(case, dir, &path_str, bytes, target),
        Kind::Parse | Kind::Stream | Kind::WbBaseline => {
            let run = |tag: &str| -> Result<usize, String> {
                let n = if case.kind == Kind::Stream {
                    stream_once(&path_str)?
                } else {
                    parse_once(&path_str)?
                };
                if n == 0 {
                    return Err(format!("{}：{tag} 轮产出 0 页——fixture 或路由异常", case.name));
                }
                Ok(n)
            };
            for _ in 0..WARMUP_ROUNDS {
                run("预热")?;
            }
            let mut durs: Vec<Duration> = Vec::with_capacity(TIMED_ROUNDS);
            let mut pages = 0usize;
            for _ in 0..TIMED_ROUNDS {
                let t = Instant::now();
                pages = run("计时")?;
                durs.push(t.elapsed());
            }
            durs.sort();
            Ok((
                Row {
                    name: case.name,
                    bytes,
                    median: durs[durs.len() / 2],
                    written: None,
                    wb: case.kind == Kind::WbBaseline,
                },
                pages,
            ))
        }
    }
}

/// VmHWM：进程生命期内存高点（kB）。Linux only（/proc/self/status）；
/// 非 Linux 或读取失败返回 None（表内显示 n/a）。
fn vm_hwm_kb() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok();
        }
    }
    None
}

// ── 输出 ─────────────────────────────────────────────────────────────

/// 千分位整数
fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &c) in b.iter().enumerate() {
        if i > 0 && (b.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c as char);
    }
    out
}

fn print_table(rows: &[Row], single_mode: bool, hwm_kb: Option<u64>) {
    let parse: Vec<&Row> = rows.iter().filter(|r| !r.wb).collect();
    let wb: Vec<&Row> = rows.iter().filter(|r| r.wb).collect();
    if !parse.is_empty() {
        println!("docforge 解析吞吐基准（fixture 程序合成）");
        println!(
            "方法学：每 case 预热 {WARMUP_ROUNDS} 轮 + 计时 {TIMED_ROUNDS} 轮取中位；\
             吞吐 = 输入字节 / 中位耗时；字节 = fixture 实际大小"
        );
        println!();
        print_rows(&parse, single_mode, hwm_kb);
        println!();
    }
    if !wb.is_empty() {
        println!(
            "docforge 回写吞吐基准（回写单程：setup 的译文对构造不计入计时，\
             writeback_* 内部重解析计入；wb-*-noread = 同 fixture 只-parse 对照行）"
        );
        println!();
        print_rows(&wb, single_mode, hwm_kb);
        println!();
    }
    match hwm_kb {
        Some(kb) if single_mode => println!(
            "VmHWM = {:.1} MB：--single 模式（本进程只跑此 case），为该 case 的真实峰值\
             （解析 case 含 IR 驻留；wb case 另含 setup 整批译文对的驻留——翻译管线\
             回写前的真实形态）。",
            kb as f64 / 1024.0
        ),
        Some(kb) => println!(
            "VmHWM（累计）= {:.1} MB：多 case 同进程跑，VmHWM 是进程生命期高点、不可重置，\
             只反映全程峰值；逐 case 真实峰值请用 --single 模式逐个跑（见 --help）。",
            kb as f64 / 1024.0
        ),
        None => println!("VmHWM：n/a（非 Linux 或读取 /proc/self/status 失败）"),
    }
}

/// 一张表（解析族 / 回写族同列结构；任一行带 written 则加 written 列——回写族）。
/// 列宽 16 起：容纳最长 case 名 wb-docx-noread。
fn print_rows(rows: &[&Row], single_mode: bool, hwm_kb: Option<u64>) {
    let show_written = rows.iter().any(|r| r.written.is_some());
    if show_written {
        println!(
            "{:<16} {:>14} {:>12} {:>12}  {:>8}  VmHWM",
            "case", "输入字节", "中位耗时", "吞吐", "written"
        );
        println!(
            "{} {} {} {}  {}  {}",
            "-".repeat(16),
            "-".repeat(14),
            "-".repeat(12),
            "-".repeat(12),
            "-".repeat(8),
            "-".repeat(10)
        );
    } else {
        println!(
            "{:<16} {:>14} {:>12} {:>12}  VmHWM",
            "case", "输入字节", "中位耗时", "吞吐"
        );
        println!(
            "{} {} {} {}  {}",
            "-".repeat(16),
            "-".repeat(14),
            "-".repeat(12),
            "-".repeat(12),
            "-".repeat(10)
        );
    }
    for row in rows {
        let secs = row.median.as_secs_f64();
        let mbs = row.bytes as f64 / secs / (1024.0 * 1024.0);
        let hwm_col = match (single_mode, hwm_kb) {
            (true, Some(kb)) => format!("{:.1} MB", kb as f64 / 1024.0),
            (true, None) => "n/a".to_string(),
            (false, _) => "-".to_string(),
        };
        if show_written {
            let written = row.written.map_or_else(|| "-".to_string(), |w| w.to_string());
            println!(
                "{:<16} {:>14} {:>12} {:>12}  {:>8}  {}",
                row.name,
                fmt_int(row.bytes),
                format!("{:.1} ms", secs * 1000.0),
                format!("{mbs:.1} MB/s"),
                written,
                hwm_col,
            );
        } else {
            println!(
                "{:<16} {:>14} {:>12} {:>12}  {}",
                row.name,
                fmt_int(row.bytes),
                format!("{:.1} ms", secs * 1000.0),
                format!("{mbs:.1} MB/s"),
                hwm_col,
            );
        }
    }
}

// ── 入口 ─────────────────────────────────────────────────────────────

const HELP: &str = "\
docforge 解析/回写吞吐内存基准（数字与方法学见 BENCHMARK.md）

用法：
  cargo bench --bench throughput                      # 全部 case（默认模式）
  cargo bench --bench throughput -- --single txt      # 单进程只跑一个 case
  cargo bench --bench throughput -- --list            # 列出当前编译面可用 case

参数：
  --single <case>   只跑指定 case。VmHWM 是进程生命期高点、不可重置——逐 case 真实峰值
                    必须单进程单 case（外层脚本逐 case 调用；--list 输出可直接喂 for 循环）
  --list            每行打印一个 case 名
  --help            本说明

case 族：
  解析族（txt/md/html/csv/docx/epub）  吞吐 = 输入字节 / parse 中位耗时
  txt-stream / md-stream               同 fixture 的 parse_streaming 对照
  回写族（wb-*）                       吞吐 = 输入字节 / 回写单程中位耗时（setup 的
                                       parse→单元→假译文对构造不计入计时；writeback_*
                                       内部按对齐契约重解析，计入）。wb-docx/wb-epub/
                                       wb-xlsx fixture 各含 1MB 伪二进制条目（raw_copy
                                       直拷路径）；written 列 = 实际写入单元数（≠ 送译数
                                       即判错，防静默零写入的假吞吐）；wb-docx-noread =
                                       同 fixture 只-parse 的对照行，量化回写增量成本。

方法学：fixture 程序合成（确定性模式循环、跑时写入临时目录、结束清理）；
每 case 预热 1 轮 + 计时 3 轮取中位；吞吐 = 输入字节 / 中位耗时；
峰值内存读 /proc/self/status 的 VmHWM（非 Linux 显示 n/a）。
docx / epub / wb-* 容器 case 需对应 feature（如 --features docx,epub,xlsx）。";

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    eprintln!("（--help 查看用法）");
    std::process::exit(2);
}

fn main() {
    // 参数解析
    let mut single: Option<String> = None;
    let mut list = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return;
            }
            "--list" => list = true,
            // cargo 会给 harness=false 的 bench 二进制附加 libtest 风格的 --bench，
            // 本程序不区分该标记、直接忽略
            "--bench" => {}
            "--single" => {
                single = Some(it.next().unwrap_or_else(|| die("--single 需要一个 case 名")));
            }
            other => die(&format!("未知参数: {other}")),
        }
    }
    let cases = all_cases();
    if list {
        for c in &cases {
            println!("{}", c.name);
        }
        return;
    }
    let single_mode = single.is_some();
    let selected: Vec<Case> = match &single {
        None => cases,
        Some(name) => {
            let picked = cases.into_iter().find(|c| c.name == name.as_str());
            match picked {
                Some(c) => vec![c],
                None => {
                    let avail: Vec<&str> = all_cases().iter().map(|c| c.name).collect();
                    die(&format!(
                        "未知 case: {name}（当前编译面可用：{}）",
                        avail.join(" ")
                    ));
                }
            }
        }
    };

    // fixture 生成：临时目录、跑完整体清理
    let dir = std::env::temp_dir().join(format!("docforge_bench_{}", std::process::id()));
    if let Err(e) = fs::create_dir_all(&dir) {
        die(&format!("创建临时目录 {} 失败: {e}", dir.display()));
    }
    let mut prepared: Vec<&'static str> = Vec::new();
    for case in &selected {
        if prepared.contains(&case.file) {
            continue;
        }
        if let Err(e) = ensure_fixture(&dir, case.file) {
            let _ = fs::remove_dir_all(&dir);
            die(&format!("生成 fixture {} 失败: {e}", case.file));
        }
        prepared.push(case.file);
    }

    // 跑 case
    let mut rows: Vec<Row> = Vec::with_capacity(selected.len());
    for case in &selected {
        match run_case(case, &dir) {
            Ok((row, detail_n)) => {
                let detail = match row.written {
                    Some(w) => format!("written {w} 单元"),
                    None => format!("{} 页", detail_n),
                };
                println!(
                    "  已跑 {}：{} 字节，{}，中位 {:.1} ms",
                    row.name,
                    fmt_int(row.bytes),
                    detail,
                    row.median.as_secs_f64() * 1000.0
                );
                rows.push(row);
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&dir);
                die(&e);
            }
        }
    }

    // VmHWM 在全部 case 结束后读（进程生命期高点此时已定型）
    let hwm = vm_hwm_kb();
    let _ = fs::remove_dir_all(&dir);
    print_table(&rows, single_mode, hwm);
}
