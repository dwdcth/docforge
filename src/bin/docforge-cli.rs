//! docforge-cli：全格式文档转换命令行（专注输出 GFM 或 Google OKF v0.3，内置双轻量化纯 Rust OCR）。
//!
//! # 用法
//!
//! ```text
//! docforge-cli <输入>... [选项]
//! ```
//!
//! 选项：
//!
//! ```text
//!   -f, --format gfm|okf         输出格式（默认 gfm）
//!   --okf-type <类型>            OKF frontmatter 的 type 字段（默认 Document）
//!   --okf-title <标题>           OKF 单文档标题或 Bundle 根标题
//!   --ocr <模式>                 OCR 引擎选择（默认 ppocr，懒加载，纯数字文档零开销）：
//!                                  ppocr        PP-OCRv6 Small（纯 Rust qppocr，默认）
//!                                  ppocr-tiny   PP-OCRv6 Tiny（~6.3 MB 极轻）
//!                                  ppocr-medium PP-OCRv6 Medium（高精度）
//!                                  indra        Indra OCR（纯 Rust fusor-ai，支持版面/纠偏/数学公式 LaTeX）
//!                                  off          关闭 OCR
//!   --ocr-model-dir <目录>       指定离线/自定义 OCR 模型目录（默认平台缓存目录，首次按需自动下载）
//!   --ocr-mirror auto|cn|global  模型下载镜像策略（默认 auto：探测本机 IP 中国大陆/境外自动选源）
//!   --ocr-images                 同时对 DOCX/EPUB/CHM/FB2 内的嵌图执行 OCR
//!   --ocr-no-math                关闭 Indra OCR 的数学公式 → LaTeX 识别（提速）
//!   -o, --out <文件>             单文件输出路径（默认 stdout）
//!   --out-dir <目录>             批量输出目录（多输入/目录时必需；OKF 模式自动生成 index.md 与 assets/）
//!   --list-formats               打印支持的输入格式与 OCR 引擎后退出
//!   -h, --help                   显示帮助信息
//!   -V, --version                显示版本号
//! ```

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use docforge::ocr::download::MirrorPolicy;
use docforge::ocr::indra::{IndraConfig, IndraOcrEngine};
use docforge::ocr::ppocr::{PpOcrConfig, PpOcrEngine, PpOcrTier};
use docforge::ocr::OcrEngine;

/// 目录递归收集的扩展名白名单（小写、不含点，由 `cli` feature 保证全格式已编译）。
const SUPPORTED_EXTENSIONS: &[&str] = &[
    // 文本与字幕
    "txt", "srt", "vtt", "ass", "ssa", "md", "markdown", "csv", "html", "htm", "rtf",
    // PDF
    "pdf",
    // 现代 Office（OOXML + 含宏/模板变体）
    "docx", "docm", "dotx", "dotm",
    "xlsx", "xlsm", "xltx", "xltm",
    "pptx", "pptm", "ppsx", "ppsm", "potx", "potm",
    // 老式二进制 Office（OLE2 / XLSB）
    "doc", "ppt", "pps", "pot", "xls", "xlsb",
    // OpenDocument
    "odt", "ods", "odp",
    // 电子书与帮助文档
    "epub", "fb2", "mobi", "prc", "azw3", "chm", "cbz", "djvu",
    // 压缩包容器
    "zip", "rar", "7z",
    // 图像直读
    "png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OcrChoice {
    Off,
    PpOcr(PpOcrTier),
    Indra,
}

impl OcrChoice {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" | "0" => Some(Self::Off),
            "ppocr" | "ppocr-small" | "ppocrv6" | "auto" | "on" => {
                Some(Self::PpOcr(PpOcrTier::Small))
            }
            "ppocr-tiny" => Some(Self::PpOcr(PpOcrTier::Tiny)),
            "ppocr-medium" => Some(Self::PpOcr(PpOcrTier::Medium)),
            "indra" | "indra-ocr" => Some(Self::Indra),
            _ => None,
        }
    }
}

fn help_text() -> &'static str {
    concat!(
        "用法：docforge-cli <输入>... [选项]\n\n",
        "将任意支持的文档转换为标准 GitHub-Flavored Markdown (GFM) 或 Google Open Knowledge Format (OKF v0.3)。\n",
        "内置双轻量化纯 Rust OCR（PP-OCRv6 与 Indra OCR），首次遇到扫描件/图片时按 IP 归属地自动选镜像下载模型。\n\n",
        "选项：\n",
        "  -f, --format gfm|okf         输出格式（默认 gfm）\n",
        "  --okf-type <类型>            OKF frontmatter 的 type 字段（默认 Document）\n",
        "  --okf-title <标题>           OKF 单文档标题或 Bundle 根标题\n",
        "  --ocr <模式>                 OCR 引擎：ppocr（默认）| ppocr-tiny | ppocr-medium | indra | off\n",
        "  --ocr-model-dir <目录>       指定离线/自定义模型目录（也可用 DOCFORGE_OCR_MODEL_DIR）\n",
        "  --ocr-mirror auto|cn|global  下载镜像策略（默认 auto：探测 IP 中国大陆走镜像加速，境外走官方源）\n",
        "  --ocr-images                 同时对 DOCX/EPUB/CHM/FB2 内嵌图片执行 OCR\n",
        "  --ocr-no-math                关闭 Indra OCR 的数学公式 → LaTeX 识别\n",
        "  -o, --out <文件>             单文件输出路径（默认 stdout）\n",
        "  --out-dir <目录>             输出目录（多输入或目录批量时必需；OKF 模式自动生成 index.md 与 assets/）\n",
        "  --list-formats               列出支持的所有输入格式与 OCR 引擎\n",
        "  -h, --help                   显示本帮助\n",
        "  -V, --version                显示版本号\n\n",
        "退出码：0=全部成功  1=参数错误或全部失败  2=部分失败"
    )
}

fn usage_err() -> ! {
    eprintln!("{}", help_text());
    std::process::exit(1);
}

struct Job {
    input: PathBuf,
    rel_dir: PathBuf,
    stem: String,
    orig_ext: String,
}

fn print_formats() {
    println!(
        "docforge-cli {} 支持的输入格式与 OCR 引擎（全格式已启用）：\n",
        env!("CARGO_PKG_VERSION")
    );
    println!("[文本与标记]");
    println!("  txt                       纯文本（含自动中文编码探测与章节切分）");
    println!("  md markdown               Markdown（GFM + frontmatter）");
    println!("  html htm                  HTML（含 MathML → LaTeX）");
    println!("  csv                       CSV 表格");
    println!("  rtf                       RTF（含中文 GBK/CP936）");
    println!("  srt vtt ass ssa           字幕");
    println!("\n[Office 文档]");
    println!("  docx docm dotx dotm       Word OOXML（含 OMML → LaTeX 与样式继承）");
    println!("  xlsx xlsm xltx xltm       Excel OOXML");
    println!("  pptx pptm ppsx ppsm potx  PowerPoint OOXML");
    println!("  doc                       Word 97-2003 二进制");
    println!("  ppt pps pot               PowerPoint 97-2003 二进制");
    println!("  xls                       Excel 97-2003 BIFF");
    println!("  xlsb                      Excel 二进制工作簿");
    println!("  odt ods odp               OpenDocument（含 MathML → LaTeX）");
    println!("\n[PDF / 电子书 / 容器 / 图像]");
    println!("  pdf                       PDF 页级混合路由（文字层直提 + 扫描页 OCR）");
    println!("  epub                      EPUB 电子书（含内嵌图片资产）");
    println!("  fb2 (+fb2.zip)            FictionBook 电子书");
    println!("  mobi prc azw3             Kindle MOBI7 / KF8 电子书");
    println!("  chm                       Microsoft CHM 帮助文档");
    println!("  djvu                      DjVu（文字层 + 扫描页 OCR）");
    println!("  cbz                       漫画压缩包");
    println!("  zip rar 7z                压缩包容器（内层文档多页合并）");
    println!("  png jpg jpeg webp bmp tif tiff  图像直读 + OCR");
    println!("\n[内置轻量化纯 Rust OCR 引擎（--ocr）]");
    println!("  ppocr（默认）             PP-OCRv6 Small（qppocr 纯 Rust 内核，~31 MB 按需下载，Apache-2.0 权重）");
    println!("  ppocr-tiny / ppocr-medium PP-OCRv6 Tiny（~6.3 MB）/ Medium（~139 MB）");
    println!("  indra                     Indra OCR（fusor-ai 纯 Rust，纠偏/版面/公式转 LaTeX，~92 MB 按需下载）");
    println!("  off                       关闭 OCR");
    println!("\n[输出格式（--format）]");
    println!("  gfm                       GitHub-Flavored Markdown（默认）");
    println!("  okf                       Google Open Knowledge Format v0.3（frontmatter + assets/ + index.md）");
}

fn make_ocr_engine(
    choice: OcrChoice,
    model_dir: Option<PathBuf>,
    mirror: MirrorPolicy,
    indra_math: bool,
) -> Option<Arc<dyn OcrEngine>> {
    match choice {
        OcrChoice::Off => None,
        OcrChoice::PpOcr(tier) => Some(PpOcrEngine::shared(PpOcrConfig {
            tier,
            model_dir,
            mirror,
        })),
        OcrChoice::Indra => Some(IndraOcrEngine::shared(IndraConfig {
            model_dir,
            mirror,
            math: indra_math,
        })),
    }
}

/// 装配全格式引擎：`extract_images` 开启时保留内嵌图片字节（用于 OKF 落盘或 `--ocr-images`）；
/// `ocr_engine` 存在时注册 `ocr-images` 能力并注入 PDF/DjVu（懒加载，纯数字文档零开销）。
fn build_engine(
    extract_images: bool,
    ocr_engine: Option<Arc<dyn OcrEngine>>,
) -> docforge::DocumentEngine {
    let mut registry = docforge::Registry::new();
    docforge::plugins::text::register_defaults(&mut registry);

    let office_opts = docforge::backends::office::OfficeOptions::new()
        .with_ocr_images(extract_images);
    docforge::backends::office::register_with(&mut registry, &office_opts);
    docforge::backends::legacy_office::register_with_ocr_images(&mut registry, extract_images);
    docforge::backends::odf::register_defaults(&mut registry);
    docforge::backends::epub::register_with_ocr_images(&mut registry, extract_images);
    docforge::backends::archive::register_defaults(&mut registry);
    docforge::backends::chm::register_with_ocr_images(&mut registry, extract_images);
    docforge::backends::cbz::register_defaults(&mut registry);
    docforge::backends::image_files::register_defaults(&mut registry);
    docforge::backends::fb2::register_with_ocr_images(&mut registry, extract_images);
    docforge::backends::mobi::register_defaults(&mut registry);

    if let Some(ref eng) = ocr_engine {
        docforge::ocr::images::register(&mut registry, Arc::clone(eng));
    }

    docforge::backends::djvu::register(
        &mut registry,
        docforge::backends::djvu::DjVuOptions {
            engine: ocr_engine.clone(),
        },
    );
    docforge::backends::pdf::register(
        &mut registry,
        docforge::backends::pdf::PdfOptions {
            mode: if ocr_engine.is_some() {
                docforge::backends::pdf::OcrMode::Auto
            } else {
                docforge::backends::pdf::OcrMode::Off
            },
            engine: ocr_engine,
            ..Default::default()
        },
    );

    docforge::DocumentEngine::new(registry)
}

fn convert_one_okf(
    engine: &docforge::DocumentEngine,
    input: &Path,
    out_path: Option<&Path>,
    rel_out_path: Option<String>,
    okf_type: &str,
    title_override: Option<&str>,
) -> Option<docforge::okf::OkfIndexEntry> {
    let doc = match engine.parse(input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}: 解析失败：{e}", input.display());
            return None;
        }
    };
    if let Some(w) = doc.meta.extra.get("warnings") {
        if !w.trim().is_empty() {
            eprintln!("{}: 警告：{w}", input.display());
        }
    }
    let source_last_modified = std::fs::metadata(input)
        .and_then(|m| m.modified())
        .ok()
        .map(docforge::okf::format_rfc3339_utc);
    let asset_prefix = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "doc".to_string());
    let resource = input
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| input.display().to_string());

    let opts = docforge::okf::OkfOptions {
        concept_type: okf_type.to_string(),
        title: title_override.map(|s| s.to_string()),
        resource: Some(resource),
        asset_prefix: Some(asset_prefix),
        include_assets: out_path.is_some(),
        source_last_modified,
        ..Default::default()
    };
    let concept = docforge::okf::to_okf_concept(&doc, &opts);

    match out_path {
        Some(p) => {
            if let Err(e) = std::fs::write(p, &concept.markdown) {
                eprintln!("{}: 写出失败（{}）：{e}", input.display(), p.display());
                return None;
            }
            if !concept.assets.is_empty() {
                let base_dir = p.parent().unwrap_or(Path::new("."));
                for asset in &concept.assets {
                    let asset_target = base_dir.join(&asset.rel_path);
                    if let Some(parent) = asset_target.parent() {
                        if let Err(e) = std::fs::create_dir_all(parent) {
                            eprintln!(
                                "{}: 创建资产目录失败（{}）：{e}",
                                input.display(),
                                parent.display()
                            );
                            return None;
                        }
                    }
                    if let Err(e) = std::fs::write(&asset_target, &asset.data) {
                        eprintln!(
                            "{}: 写出资产失败（{}）：{e}",
                            input.display(),
                            asset_target.display()
                        );
                        return None;
                    }
                }
            }
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            let _ = write!(stdout, "{}", concept.markdown);
        }
    }

    Some(docforge::okf::OkfIndexEntry {
        rel_path: rel_out_path.unwrap_or_default(),
        title: concept.title,
        description: concept.description,
        concept_type: concept.concept_type,
    })
}

fn convert_one_gfm(
    engine: &docforge::DocumentEngine,
    input: &Path,
    out_path: Option<&Path>,
) -> bool {
    let doc = match engine.parse(input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}: 解析失败：{e}", input.display());
            return false;
        }
    };
    if let Some(w) = doc.meta.extra.get("warnings") {
        if !w.trim().is_empty() {
            eprintln!("{}: 警告：{w}", input.display());
        }
    }
    let output = docforge::emit::to_gfm(&doc);
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

fn unique_target(
    claimed: &mut HashSet<String>,
    rel_dir: &Path,
    stem: &str,
    orig_ext: &str,
) -> String {
    let orig = if orig_ext.is_empty() {
        String::new()
    } else {
        format!(".{orig_ext}")
    };
    let mut attempt = 0u32;
    loop {
        let name = match attempt {
            0 => format!("{stem}.md"),
            1 => format!("{stem}{orig}.md"),
            n => format!("{stem}{orig}_{n}.md"),
        };
        let key = rel_dir.join(&name).to_string_lossy().into_owned();
        if claimed.insert(key) {
            return name;
        }
        attempt += 1;
    }
}

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
    }
}

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut format = "gfm".to_string();
    let mut okf_type = "Document".to_string();
    let mut okf_title: Option<String> = None;
    let mut ocr_choice = OcrChoice::PpOcr(PpOcrTier::Small);
    let mut ocr_model_dir: Option<PathBuf> = None;
    let mut ocr_mirror = MirrorPolicy::Auto;
    let mut ocr_images = false;
    let mut ocr_math = true;
    let mut out_path: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut list_formats = false;

    let mut i = 0;
    while i < args.len() {
        let view = args[i].to_string_lossy();
        match view.as_ref() {
            "-h" | "--help" => {
                println!("{}", help_text());
                return;
            }
            "-V" | "--version" => {
                println!("docforge-cli {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--format" | "-f" => {
                i += 1;
                format = args
                    .get(i)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| usage_err());
            }
            "--okf-type" => {
                i += 1;
                okf_type = args
                    .get(i)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| usage_err());
            }
            "--okf-title" => {
                i += 1;
                okf_title = Some(
                    args.get(i)
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| usage_err()),
                );
            }
            "--ocr" => {
                i += 1;
                let raw = args
                    .get(i)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| usage_err());
                ocr_choice = match OcrChoice::parse(&raw) {
                    Some(c) => c,
                    None => {
                        eprintln!(
                            "未知 --ocr 模式：{raw}（支持 ppocr | ppocr-tiny | ppocr-medium | indra | off）"
                        );
                        std::process::exit(1);
                    }
                };
            }
            "--ocr-model-dir" => {
                i += 1;
                ocr_model_dir = Some(
                    args.get(i)
                        .map(PathBuf::from)
                        .unwrap_or_else(|| usage_err()),
                );
            }
            "--ocr-mirror" => {
                i += 1;
                let raw = args
                    .get(i)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| usage_err());
                ocr_mirror = match MirrorPolicy::parse(&raw) {
                    Some(m) => m,
                    None => {
                        eprintln!("未知 --ocr-mirror 策略：{raw}（支持 auto | cn | global）");
                        std::process::exit(1);
                    }
                };
            }
            "--ocr-images" => ocr_images = true,
            "--ocr-no-math" => ocr_math = false,
            "--out" | "-o" => {
                i += 1;
                out_path = Some(
                    args.get(i)
                        .map(PathBuf::from)
                        .unwrap_or_else(|| usage_err()),
                );
            }
            "--out-dir" => {
                i += 1;
                out_dir = Some(
                    args.get(i)
                        .map(PathBuf::from)
                        .unwrap_or_else(|| usage_err()),
                );
            }
            "--list-formats" => list_formats = true,
            s if s.starts_with('-') => usage_err(),
            _ => inputs.push(PathBuf::from(&args[i])),
        }
        i += 1;
    }

    if list_formats {
        print_formats();
        return;
    }

    if inputs.is_empty() {
        usage_err();
    }
    if !matches!(format.as_str(), "gfm" | "okf") {
        eprintln!("未知输出格式：{format}（仅支持 gfm | okf）");
        std::process::exit(1);
    }

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

    let is_okf = format == "okf";
    let extract_images = (is_okf && (out_dir.is_some() || out_path.is_some())) || ocr_images;
    let ocr_engine = make_ocr_engine(ocr_choice, ocr_model_dir, ocr_mirror, ocr_math);
    let engine = build_engine(extract_images, ocr_engine);

    let mut ok = 0usize;
    let mut bad = pre_failed.len();
    for msg in &pre_failed {
        eprintln!("{msg}");
    }

    if batch {
        let out_root = out_dir.clone().unwrap_or_else(|| PathBuf::from("out"));
        if let Err(e) = std::fs::create_dir_all(&out_root) {
            eprintln!("创建输出目录失败（{}）：{e}", out_root.display());
            std::process::exit(1);
        }
        let mut claimed: HashSet<String> = HashSet::new();
        let mut okf_entries: Vec<docforge::okf::OkfIndexEntry> = Vec::new();
        let total = jobs.len();
        for (idx, job) in jobs.iter().enumerate() {
            eprintln!("[{}/{}] {}", idx + 1, total, job.input.display());
            let stem = if is_okf {
                docforge::okf::safe_concept_stem(&job.stem)
            } else {
                job.stem.clone()
            };
            let name = unique_target(&mut claimed, &job.rel_dir, &stem, &job.orig_ext);
            let target = out_root.join(&job.rel_dir).join(&name);
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
            if is_okf {
                let rel_posix = job
                    .rel_dir
                    .join(&name)
                    .to_string_lossy()
                    .replace('\\', "/");
                match convert_one_okf(
                    &engine,
                    &job.input,
                    Some(&target),
                    Some(rel_posix),
                    &okf_type,
                    None,
                ) {
                    Some(entry) => {
                        okf_entries.push(entry);
                        ok += 1;
                    }
                    None => bad += 1,
                }
            } else if convert_one_gfm(&engine, &job.input, Some(&target)) {
                ok += 1;
            } else {
                bad += 1;
            }
        }
        if is_okf && !okf_entries.is_empty() {
            let bundle_title = okf_title.clone().unwrap_or_else(|| {
                out_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "Knowledge Bundle".to_string())
            });
            let bundle_desc = format!(
                "Knowledge bundle generated by docforge ({} documents).",
                okf_entries.len()
            );
            let indexes =
                docforge::okf::build_bundle_indexes(&bundle_title, &bundle_desc, &okf_entries);
            for (rel_idx, content) in indexes {
                let dest = out_root.join(rel_idx);
                if let Err(e) = std::fs::write(&dest, content) {
                    eprintln!("写出 OKF 索引失败（{}）：{e}", dest.display());
                    bad += 1;
                }
            }
        }
        eprintln!("完成：成功 {ok} / 失败 {bad}");
    } else if let Some(job) = jobs.first() {
        let target: Option<PathBuf> = if let Some(root) = out_dir.as_ref() {
            if let Err(e) = std::fs::create_dir_all(root) {
                eprintln!("创建输出目录失败（{}）：{e}", root.display());
                std::process::exit(1);
            }
            let mut claimed = HashSet::new();
            let stem = if is_okf {
                docforge::okf::safe_concept_stem(&job.stem)
            } else {
                job.stem.clone()
            };
            let name = unique_target(&mut claimed, Path::new(""), &stem, &job.orig_ext);
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
        if is_okf {
            let rel_posix = target
                .as_ref()
                .and_then(|t| t.file_name())
                .map(|n| n.to_string_lossy().into_owned());
            match convert_one_okf(
                &engine,
                &job.input,
                target.as_deref(),
                rel_posix,
                &okf_type,
                okf_title.as_deref(),
            ) {
                Some(entry) => {
                    if let Some(root) = out_dir.as_ref() {
                        let bundle_title = okf_title
                            .clone()
                            .unwrap_or_else(|| entry.title.clone());
                        let bundle_desc = entry.description.clone();
                        let indexes = docforge::okf::build_bundle_indexes(
                            &bundle_title,
                            &bundle_desc,
                            &[entry],
                        );
                        for (rel_idx, content) in indexes {
                            let _ = std::fs::write(root.join(rel_idx), content);
                        }
                    }
                    ok += 1;
                }
                None => bad += 1,
            }
        } else if convert_one_gfm(&engine, &job.input, target.as_deref()) {
            ok += 1;
        } else {
            bad += 1;
        }
    }

    let code = if bad == 0 {
        0
    } else if ok == 0 {
        1
    } else {
        2
    };
    std::process::exit(code);
}
