//! docforge-cli：通用文档解析命令行（M2 雏形）。
//!
//! 用法：docforge-cli <输入文件> [选项]
//!   --format md|json|text|units   输出格式（默认 md；units = 翻译单元 JSON 行）
//!   --ocr auto|on|off             PDF OCR 模式（默认 auto；需 pdf feature）
//!   --out <文件>                  输出到文件（默认 stdout）
//!   --pages a-b                   页范围（json/text/units 生效，md 全量）
//!
//! 编译能力随 crate feature：文本族默认；--features encoding 补中文编码；
//! --features pdf,ocr-oar 补 PDF 页级路由与 OCR。手写参数解析（零依赖）。

use std::io::Write;

fn usage() -> ! {
    eprintln!("用法：docforge-cli <输入文件> [--format md|json|text|units] [--ocr auto|on|off] [--out <文件>]");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input: Option<String> = None;
    let mut format = "md".to_string();
    #[cfg(feature = "pdf")]
    let mut ocr = "auto".to_string();
    #[allow(unused_mut)]
    let mut out_path: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--format" | "-f" => {
                i += 1;
                format = args.get(i).cloned().unwrap_or_else(|| usage());
            }
            "--ocr" => {
                #[cfg(feature = "pdf")]
                {
                    i += 1;
                    ocr = args.get(i).cloned().unwrap_or_else(|| usage());
                }
                #[cfg(not(feature = "pdf"))]
                {
                    eprintln!("--ocr 需要 pdf feature");
                    std::process::exit(2);
                }
            }
            "--out" | "-o" => {
                i += 1;
                out_path = args.get(i).cloned();
            }
            s if s.starts_with('-') => usage(),
            s => {
                if input.is_some() {
                    usage();
                }
                input = Some(s.to_string());
            }
        }
        i += 1;
    }
    let Some(input) = input else { usage() };

    // 装配：文本族 + 各 feature 后端
    let mut registry = docforge::Registry::new();
    docforge::plugins::text::register_defaults(&mut registry);
    #[cfg(feature = "docx")]
    docforge::backends::office::register_defaults(&mut registry);
    #[cfg(feature = "epub")]
    docforge::backends::epub::register_defaults(&mut registry);
    #[cfg(feature = "cbz")]
    docforge::backends::cbz::register_defaults(&mut registry);
    #[cfg(feature = "fb2")]
    docforge::backends::fb2::register_defaults(&mut registry);
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
        let mode = match ocr.as_str() {
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

    let engine = docforge::DocumentEngine::new(registry);
    let doc = match engine.parse(&input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("解析失败：{e}");
            std::process::exit(1);
        }
    };

    let output = match format.as_str() {
        "md" => docforge::emit::to_markdown(&doc),
        "text" => docforge::emit::to_text(&doc),
        "json" => docforge::emit::to_json_pretty(&doc),
        "units" => {
            // 翻译单元：每行一个 JSON（JSONL，翻译管线友好）
            let units = docforge::emit::to_translation_units(&doc);
            units
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
                    })
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => {
            eprintln!("未知格式：{format}（md|json|text|units）");
            std::process::exit(2);
        }
    };

    match out_path {
        Some(p) => {
            if let Err(e) = std::fs::write(&p, &output) {
                eprintln!("写出失败（{p}）：{e}");
                std::process::exit(1);
            }
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{output}");
        }
    }
}
