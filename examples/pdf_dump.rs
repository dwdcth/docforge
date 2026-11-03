//! 对表辅助（M1 验收 docforge-spec-m1 §7.5）：生成样例 PDF → 公共 API 解析
//! → dump 行集（页号/来源/置信度/坐标单位/文本），供与 rumi route_pdf 输出比对。
//!
//! 运行：cargo run --features pdf --example pdf_dump -- [已有.pdf]
//! 不带参数时自动生成两页中文文字层样例（需系统 CJK 字体）。

use docforge::backends::pdf::{register, PdfOptions};
use docforge::{DocumentEngine, Registry};

fn main() {
    let target = std::env::args().nth(1);
    let path = match target {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            eprintln!("未指定 PDF；请传入路径（对表用 rumi 探针生成的 sample.pdf）");
            std::process::exit(2);
        }
    };

    let mut registry = Registry::new();
    register(&mut registry, PdfOptions::default());
    let engine = DocumentEngine::new(registry);

    let doc = match engine.parse(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("解析失败：{e}");
            std::process::exit(1);
        }
    };
    println!("file_type={} pages={}", doc.meta.file_type, doc.meta.page_count);
    for page in &doc.pages {
        println!(
            "# page {} coord={:?} blocks={}",
            page.name.as_deref().unwrap_or("?"),
            page.coord_units,
            page.blocks.len()
        );
        for b in &page.blocks {
            println!("  [{:?} conf={:?}] {}", b.source, b.confidence, b.content);
        }
    }
}
