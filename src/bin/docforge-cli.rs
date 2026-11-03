//! docforge-cli：通用文档解析与格式转换命令行（多输入 / 目录递归 / 批量转换）。
//!
//! # 用法
//!
//! ```text
//! docforge-cli <输入>... [选项]
//! ```
//!
//! 输入为文件或目录（可多个混用）：目录递归收集当前构建支持格式的文件
//!（扩展名白名单 `SUPPORTED_EXTENSIONS`；隐藏项（`.` 开头）与 `target/`、
//! `node_modules/` 跳过；其余非隐藏文件按「不支持扩展名」计为失败并在
//! stderr 报告，不静默丢文件）。显式给出的单个文件不受白名单过滤
//!（router 兜底按纯文本解析，用户显式指定即用户意图）。
//!
//! 选项：
//!
//! ```text
//!   --format md|json|text|units|chunks   输出格式（默认 md）
//!       md      Markdown（表格渲染为 Markdown 表）
//!       json    完整 IR（缩进美化）
//!       text    纯文本
//!       units   翻译单元 JSONL（id/page/kind/text/source/confidence/lang/chapter）
//!       chunks  RAG 检索块 JSONL（id/page/chapter/text/block_ids）
//!   --ocr auto|on|off    PDF OCR 模式（默认 auto；需 pdf feature）
//!   --out <文件>         输出到文件（仅单输入；默认 stdout）
//!   --out-dir <目录>     批量输出目录（多输入/目录时必需）：
//!                        保持相对各输入根的目录结构；输出扩展名随格式
//!                       （md→.md json→.json text→.txt units/chunks→.jsonl）；
//!                        同 stem 冲突时拼接原扩展去重（见 unique_target）
//!   --list-formats       打印支持的输入格式（default / feature 门控两组）后退出
//! ```
//!
//! 批量语义：逐文件转换，单文件失败继续（stderr 记 `文件: 错误`），结束
//! 汇总 `成功 N / 失败 M`；进度行打 stderr（`[3/12] xx.md`）。
//! 退出码：0 = 全部成功；1 = 参数错误或全部失败；2 = 部分失败。
//!
//! # 示例
//!
//! ```text
//! docforge-cli note.md --format md                    # 单文件 → stdout
//! docforge-cli docs/ --format md --out-dir out/       # 目录递归批量转 md
//! docforge-cli a.pdf b.docx --format chunks --out-dir out/   # 多输入转 RAG 块
//! ```
//!
//! 编译能力随 crate feature：文本族默认；--features encoding 补中文编码；
//! --features pdf,ocr-oar 补 PDF 页级路由与 OCR。手写参数解析（零依赖）。

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 目录递归收集的扩展名白名单（小写、不含点）。
///
/// 静态清单，与 crate 支持的输入格式同步维护——**新增格式后更新此处**
///（router::sniff 按文件内容判格式，目录遍历只能按扩展名预筛）。
/// 未列入的非隐藏文件在批量模式下按失败报告（不静默跳过，避免丢文件）。
/// legacy OLE（doc/ppt/xls）当前无后端插件（router 可路由但无注册方），
/// 不在清单内。
const SUPPORTED_EXTENSIONS: &[&str] = &[
    // 恒开：txt 底座 + 字幕（无第三方依赖，随 txt 始终注册）
    "txt",
    "srt",
    "vtt",
    "ass",
    "ssa",
    #[cfg(feature = "md")]
    "md",
    "markdown",
    #[cfg(feature = "csv")]
    "csv",
    #[cfg(feature = "html")]
    "html",
    "htm",
    #[cfg(feature = "rtf")]
    "rtf",
    #[cfg(feature = "pdf")]
    "pdf",
    #[cfg(feature = "docx")]
    "docx",
    #[cfg(feature = "xlsx")]
    "xlsx",
    #[cfg(feature = "pptx")]
    "pptx",
    #[cfg(feature = "epub")]
    "epub",
    // 通用压缩包走 archive；fb2.zip（内层单 .fb2）由 fb2 后端解包
    #[cfg(any(feature = "archive", feature = "fb2"))]
    "zip",
    #[cfg(feature = "archive")]
    "rar",
    "7z",
    #[cfg(feature = "chm")]
    "chm",
    #[cfg(feature = "cbz")]
    "cbz",
    #[cfg(feature = "image-files")]
    "png",
    "jpg",
    "jpeg",
    "webp",
    "bmp",
    "tif",
    "tiff",
    #[cfg(feature = "fb2")]
    "fb2",
    #[cfg(feature = "mobi")]
    "mobi",
    "prc",
    "azw3",
    #[cfg(feature = "djvu")]
    "djvu",
    #[cfg(feature = "odt")]
    "odt",
    #[cfg(feature = "ods")]
    "ods",
    #[cfg(feature = "odp")]
    "odp",
];

fn usage() -> ! {
    eprintln!(
        "用法：docforge-cli <输入>... [--format md|json|text|units|chunks] [--ocr auto|on|off] \
[--out <文件>] [--out-dir <目录>] [--list-formats]\n\
         输入可为文件或目录（目录递归收集支持格式）；多输入/目录需 --out-dir\n\
         退出码：0=全部成功 1=参数错误或全部失败 2=部分失败"
    );
    std::process::exit(1);
}

/// 批量作业：一个待转换文件 + 其在 `--out-dir` 里的相对落点信息。
struct Job {
    /// 输入路径（解析直传 + 进度/错误行显示用；PathBuf 保非 UTF-8 文件名）
    input: PathBuf,
    /// 相对 --out-dir 的子目录（目录输入保持相对其根的子路径；单文件为空）
    rel_dir: PathBuf,
    /// 输出名主体 = 输入文件名去扩展
    stem: String,
    /// 原扩展（不含点；无扩展为空串）——同 stem 冲突时拼接去重用
    orig_ext: String,
}

/// --list-formats：按编译期 cfg 打印当前构建实际支持的输入格式。
///
/// 两组：默认（无需额外 feature）/ feature 门控（需 --features 显式开启）。
/// 静态清单——**新增格式后更新此处**（与 [`SUPPORTED_EXTENSIONS`] 同步维护）。
fn print_formats() {
    println!("docforge-cli 支持的输入格式（按当前编译 feature）：\n");
    println!("[默认]（无需额外 feature）：");
    println!("  txt              纯文本（恒开，含中文章节切分）");
    println!("  srt vtt ass ssa  字幕（恒开）");
    #[cfg(feature = "md")]
    println!("  md markdown      Markdown（feature md，default 已含）");
    #[cfg(feature = "csv")]
    println!("  csv              CSV 表格（feature csv，default 已含）");
    #[cfg(feature = "html")]
    println!("  html htm         HTML（feature html，default 已含）");
    #[cfg(feature = "rtf")]
    println!("  rtf              RTF（feature rtf，default 已含）");
    println!("\n[feature 门控]（需 --features 显式开启）：");
    #[cfg(feature = "pdf")]
    println!("  pdf              PDF 页级混合路由（feature pdf；OCR 需 +ocr-oar，--ocr 控制模式）");
    #[cfg(feature = "docx")]
    println!("  docx             Word（feature docx）");
    #[cfg(feature = "xlsx")]
    println!("  xlsx             Excel（feature xlsx）");
    #[cfg(feature = "pptx")]
    println!("  pptx             PowerPoint（feature pptx）");
    #[cfg(feature = "epub")]
    println!("  epub             EPUB 电子书（feature epub）");
    #[cfg(feature = "fb2")]
    println!("  fb2 (+fb2.zip)   FictionBook 电子书（feature fb2）");
    #[cfg(feature = "mobi")]
    println!("  mobi prc azw3    Kindle/MOBI7 电子书（feature mobi）");
    #[cfg(feature = "djvu")]
    println!("  djvu             DjVu 扫描文档（feature djvu）");
    #[cfg(feature = "archive")]
    println!("  zip rar 7z       压缩包容器（feature archive；内层文本多页合并）");
    #[cfg(all(feature = "fb2", not(feature = "archive")))]
    println!("  zip（仅 .fb2.zip）压缩包内层 FB2（feature fb2）");
    #[cfg(feature = "chm")]
    println!("  chm              CHM 帮助文档（feature chm）");
    #[cfg(feature = "cbz")]
    println!("  cbz              漫画压缩包（feature cbz）");
    #[cfg(feature = "image-files")]
    println!("  png jpg jpeg webp bmp tif tiff  图像直读（feature image-files）");
    #[cfg(feature = "odt")]
    println!("  odt              ODF 文本（feature odt/odf）");
    #[cfg(feature = "ods")]
    println!("  ods              ODF 表格（feature ods/odf）");
    #[cfg(feature = "odp")]
    println!("  odp              ODF 演示（feature odp/odf）");
    #[cfg(not(any(
        feature = "pdf",
        feature = "docx",
        feature = "xlsx",
        feature = "pptx",
        feature = "epub",
        feature = "fb2",
        feature = "mobi",
        feature = "djvu",
        feature = "archive",
        feature = "chm",
        feature = "cbz",
        feature = "image-files",
        feature = "odt",
        feature = "ods",
        feature = "odp"
    )))]
    println!("  （本构建未启用任何门控格式）");
}

/// 装配引擎：文本族恒注册 + 各 feature 后端。`ocr` 仅影响 PDF（需 pdf feature）。
fn build_engine(ocr: &str) -> docforge::DocumentEngine {
    #[cfg(not(feature = "pdf"))]
    let _ = ocr;
    let mut registry = docforge::Registry::new();
    docforge::plugins::text::register_defaults(&mut registry);
    #[cfg(feature = "docx")]
    docforge::backends::office::register_defaults(&mut registry);
    #[cfg(feature = "epub")]
    docforge::backends::epub::register_defaults(&mut registry);
    #[cfg(feature = "archive")]
    docforge::backends::archive::register_defaults(&mut registry);
    #[cfg(feature = "chm")]
    docforge::backends::chm::register_defaults(&mut registry);
    #[cfg(feature = "cbz")]
    docforge::backends::cbz::register_defaults(&mut registry);
    #[cfg(feature = "image-files")]
    docforge::backends::image_files::register_defaults(&mut registry);
    #[cfg(feature = "fb2")]
    docforge::backends::fb2::register_defaults(&mut registry);
    #[cfg(feature = "mobi")]
    docforge::backends::mobi::register_defaults(&mut registry);
    #[cfg(feature = "djvu")]
    docforge::backends::djvu::register(
        &mut registry,
        docforge::backends::djvu::DjVuOptions {
            #[cfg(feature = "ocr-oar")]
            engine: Some(docforge::ocr::oar::OarOcrEngine::shared()),
            #[cfg(not(feature = "ocr-oar"))]
            engine: None,
        },
    );
    #[cfg(feature = "pdf")]
    {
        use docforge::backends::pdf::{OcrMode, PdfOptions};
        let mode = match ocr {
            "on" => OcrMode::On,
            "off" => OcrMode::Off,
            "auto" => OcrMode::Auto,
            _ => usage(),
        };
        let opts = PdfOptions {
            mode,
            #[cfg(feature = "ocr-oar")]
            engine: Some(docforge::ocr::oar::OarOcrEngine::shared()),
            #[cfg(not(feature = "ocr-oar"))]
            engine: None,
            ..Default::default()
        };
        docforge::backends::pdf::register(&mut registry, opts);
    }
    docforge::DocumentEngine::new(registry)
}

/// 输出扩展名映射：md→.md json→.json text→.txt units/chunks→.jsonl。
fn format_ext(format: &str) -> &'static str {
    match format {
        "md" => "md",
        "json" => "json",
        "text" => "txt",
        "units" | "chunks" => "jsonl",
        _ => "md", // main 已校验，不可达；兜底不 panic
    }
}

/// Document → 目标格式文本。未知格式返回 None（main 已预先校验，双保险）。
fn render(format: &str, doc: &docforge::Document) -> Option<String> {
    Some(match format {
        "md" => docforge::emit::to_markdown(doc),
        "text" => docforge::emit::to_text(doc),
        "json" => docforge::emit::to_json_pretty(doc),
        "units" => {
            // 翻译单元：每行一个 JSON（JSONL，翻译管线友好）
            docforge::emit::to_translation_units(doc)
                .iter()
                .map(|u| {
                    serde_json::json!({
                        "id": u.id,
                        "page": u.page,
                        "kind": format!("{:?}", u.kind),
                        "text": u.text,
                        "source": format!("{:?}", u.source),
                        "confidence": u.confidence,
                        "lang": u.lang,
                        "chapter": u.chapter,
                        "header": u.header,
                    })
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "chunks" => {
            // RAG 检索块：每行一个 JSON（与 units 同形态，喂向量化/检索管线）
            docforge::emit::to_rag_chunks(doc)
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "page": c.page,
                        "chapter": c.chapter,
                        "text": c.text,
                        "block_ids": c.block_ids,
                    })
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => return None,
    })
}

/// 单文件转换：解析 → 按格式 emit → 写 `out_path`（None = stdout）。
/// 返回是否成功；失败详情已打 stderr（`{input}: 解析失败：…` / `…：写出失败：…`）。
fn convert_one(
    engine: &docforge::DocumentEngine,
    input: &Path,
    format: &str,
    out_path: Option<&Path>,
) -> bool {
    let doc = match engine.parse(input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}: 解析失败：{e}", input.display());
            return false;
        }
    };
    let Some(output) = render(format, &doc) else {
        eprintln!("{}: 未知格式：{format}（md|json|text|units|chunks）", input.display());
        return false;
    };
    match out_path {
        Some(p) => match std::fs::write(p, &output) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("{}: 写出失败（{}）：{e}", input.display(), p.display());
                false
            }
        },
        None => {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{output}");
            true
        }
    }
}

/// 计算唯一输出文件名：默认 `stem.{输出扩展}`；同 stem 已被本批占用时拼接
/// 原扩展去重（如 a.md 与 a.txt 同转 md：先到者得 `a.md`，后者得 `a.txt.md`）；
/// 连拼后仍冲突（如目录里同时有 a.md 与 a.txt.md）加数字后缀兜底。
/// `claimed` 跨整批共享（键 = rel_dir/name），保证 out_dir 内不互相覆盖。
fn unique_target(
    claimed: &mut HashSet<String>,
    rel_dir: &Path,
    stem: &str,
    orig_ext: &str,
    out_ext: &str,
) -> String {
    let orig = if orig_ext.is_empty() {
        String::new()
    } else {
        format!(".{orig_ext}")
    };
    let mut attempt = 0u32;
    loop {
        let name = match attempt {
            0 => format!("{stem}.{out_ext}"),
            1 => format!("{stem}{orig}.{out_ext}"),
            n => format!("{stem}{orig}_{n}.{out_ext}"),
        };
        let key = rel_dir.join(&name).to_string_lossy().into_owned();
        if claimed.insert(key) {
            return name;
        }
        attempt += 1;
    }
}

/// 递归收集目录下白名单扩展名的文件。隐藏项（`.` 开头）与 `target/`、
/// `node_modules/` 跳过；目录符号链接不跟随（`file_type()` 不解引用，
/// 无限递归免疫）。其余非隐藏文件记入 `pre_failed`（不支持的扩展名，
/// 批量语义下计为失败而非静默跳过，避免丢文件）。已见路径（跨输入去重）
/// 记入 `seen`（canonical 路径）。
fn collect_dir(
    root: &Path,
    dir: &Path,
    jobs: &mut Vec<Job>,
    pre_failed: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        pre_failed.push(format!("{}: 读取目录失败", dir.display()));
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            if name == "target" || name == "node_modules" {
                continue;
            }
            collect_dir(root, &path, jobs, pre_failed, seen);
        } else if ft.is_file() {
            let key = std::fs::canonicalize(&path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            if !seen.insert(key) {
                continue;
            }
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(name);
            let orig_ext = path
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            if SUPPORTED_EXTENSIONS.contains(&orig_ext.to_ascii_lowercase().as_str()) {
                let rel_dir = path
                    .parent()
                    .and_then(|p| p.strip_prefix(root).ok())
                    .map(|p| p.to_path_buf())
                    .unwrap_or_default();
                jobs.push(Job {
                    input: path.clone(),
                    rel_dir,
                    stem,
                    orig_ext,
                });
            } else if orig_ext.is_empty() {
                pre_failed.push(format!("{}: 不支持的扩展名（无扩展名）", path.display()));
            } else {
                pre_failed.push(format!(
                    "{}: 不支持的扩展名（.{orig_ext}）",
                    path.display()
                ));
            }
        }
        // 其余（符号链接等非常规项）：跳过
    }
}

fn main() {
    // 参数走 OsString（`args()` 遇非 UTF-8 argv 直接 panic；路径位可能非
    // UTF-8）。旗标与本 CLI 的字面量恒 ASCII，lossy 视图只用于旗标匹配/显示
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut format = "md".to_string();
    #[cfg(feature = "pdf")]
    let mut ocr = "auto".to_string();
    let mut out_path: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut list_formats = false;

    let mut i = 0;
    while i < args.len() {
        let view = args[i].to_string_lossy();
        match view.as_ref() {
            "--format" | "-f" => {
                i += 1;
                format = args
                    .get(i)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| usage());
            }
            "--ocr" => {
                #[cfg(feature = "pdf")]
                {
                    i += 1;
                    ocr = args
                        .get(i)
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| usage());
                }
                #[cfg(not(feature = "pdf"))]
                {
                    eprintln!("--ocr 需要 pdf feature");
                    std::process::exit(1);
                }
            }
            "--out" | "-o" => {
                i += 1;
                out_path = args.get(i).map(PathBuf::from);
            }
            "--out-dir" => {
                i += 1;
                out_dir = args.get(i).map(PathBuf::from);
            }
            "--list-formats" => list_formats = true,
            s if s.starts_with('-') => usage(),
            _ => inputs.push(PathBuf::from(&args[i])),
        }
        i += 1;
    }

    if list_formats {
        print_formats();
        return;
    }

    if inputs.is_empty() {
        usage();
    }
    if !matches!(format.as_str(), "md" | "json" | "text" | "units" | "chunks") {
        eprintln!("未知格式：{format}（md|json|text|units|chunks）");
        std::process::exit(1);
    }

    // 收集批量作业：目录递归（白名单过滤）+ 显式文件（不受白名单过滤，
    // 兜底按纯文本解析；跨输入按 canonical 路径去重）
    let mut jobs: Vec<Job> = Vec::new();
    let mut pre_failed: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut any_dir = false;
    for input in &inputs {
        let p = input.as_path();
        if p.is_dir() {
            any_dir = true;
            collect_dir(p, p, &mut jobs, &mut pre_failed, &mut seen);
        } else if p.is_file() {
            let key = std::fs::canonicalize(p)
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_else(|_| input.display().to_string());
            if !seen.insert(key) {
                continue;
            }
            jobs.push(Job {
                input: p.to_path_buf(),
                rel_dir: PathBuf::new(),
                stem: p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| input.display().to_string()),
                orig_ext: p
                    .extension()
                    .map(|e| e.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            });
        } else {
            pre_failed.push(format!("{}: 路径不存在", input.display()));
        }
    }
    jobs.sort_by(|a, b| a.input.cmp(&b.input));

    // 批量判定按输入形态（多输入或含目录），与实际文件数无关：
    // 目录输入即使只含 1 个文件也是批量（结果落盘而非 stdout）
    let batch = inputs.len() > 1 || any_dir;
    if batch && out_dir.is_none() {
        eprintln!("多输入/目录转换需要 --out-dir <目录>（单文件可 --out 或 stdout）");
        std::process::exit(1);
    }
    if batch && out_path.is_some() {
        eprintln!("--out 仅支持单个输入文件；多输入请用 --out-dir");
        std::process::exit(1);
    }
    if jobs.is_empty() && pre_failed.is_empty() {
        eprintln!("没有可转换的输入文件");
        std::process::exit(1);
    }

    #[cfg(feature = "pdf")]
    let engine = build_engine(&ocr);
    #[cfg(not(feature = "pdf"))]
    let engine = build_engine("");

    let mut ok = 0usize;
    let mut bad = pre_failed.len();
    for msg in &pre_failed {
        eprintln!("{msg}");
    }
    let out_ext = format_ext(&format);

    if batch {
        let out_root = out_dir.clone().unwrap_or_else(|| PathBuf::from("out"));
        if let Err(e) = std::fs::create_dir_all(&out_root) {
            eprintln!("创建输出目录失败（{}）：{e}", out_root.display());
            std::process::exit(1);
        }
        let mut claimed: HashSet<String> = HashSet::new();
        let total = jobs.len();
        for (idx, job) in jobs.iter().enumerate() {
            eprintln!("[{}/{}] {}", idx + 1, total, job.input.display());
            let name = unique_target(&mut claimed, &job.rel_dir, &job.stem, &job.orig_ext, out_ext);
            let target = out_root.join(&job.rel_dir).join(name);
            if let Some(parent) = target.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!(
                        "{}: 创建输出子目录失败（{}）：{e}",
                        job.input.display(),
                        parent.display()
                    );
                    bad += 1;
                    continue;
                }
            }
            if convert_one(&engine, &job.input, &format, Some(&target)) {
                ok += 1;
            } else {
                bad += 1;
            }
        }
        eprintln!("完成：成功 {ok} / 失败 {bad}");
    } else if let Some(job) = jobs.first() {
        // 单输入：--out 文件 / --out-dir 目录 / stdout（现状保持）。
        // jobs 为空（输入不存在，已在 pre_failed 报告）则跳过，落全败退出。
        let target: Option<PathBuf> = if let Some(root) = out_dir.as_ref() {
            if let Err(e) = std::fs::create_dir_all(root) {
                eprintln!("创建输出目录失败（{}）：{e}", root.display());
                std::process::exit(1);
            }
            let mut claimed = HashSet::new();
            let name = unique_target(&mut claimed, Path::new(""), &job.stem, &job.orig_ext, out_ext);
            Some(root.join(name))
        } else {
            out_path
        };
        if let Some(t) = &target {
            if let Some(parent) = t.parent().filter(|p| !p.as_os_str().is_empty()) {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!("创建输出目录失败（{}）：{e}", parent.display());
                    std::process::exit(1);
                }
            }
        }
        if convert_one(&engine, &job.input, &format, target.as_deref()) {
            ok += 1;
        } else {
            bad += 1;
        }
    }

    let code = if bad == 0 {
        0
    } else if ok == 0 {
        1 // 全败（含单文件失败）
    } else {
        2 // 部分失败
    };
    std::process::exit(code);
}
