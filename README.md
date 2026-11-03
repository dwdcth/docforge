# docforge

通用文档解析 crate：docparser 式统一块级 IR + 格式路由（magic 嗅探 + 后端回退链）
+ **PDF 页级混合路由**（逐页判定"文字层直提 vs 渲染+OCR"，设计参考 rumi-ocr 的
pdf_route）。OCR 置信度与块来源进 IR；输出 JSON / Markdown / 翻译友好分段，
服务解析转换、OCR、翻译三类消费者。独立项目，与 rumi-ocr 无依赖关系。

## 状态

| 期 | 内容 | 状态 |
| --- | --- | --- |
| M0 | 工程骨架 + docparser-core 吸收 + IR 定稿（confidence/source/coord_units） | ✅ |
| M1 | PDF 后端（页级路由 + OcrEngine 注入式抽象） | ✅ |
| M2 | 文本族增强（编码链/中文章节/翻译单元/emit/cli/lang-detect）+ DOCX/EPUB + HTML 修补 | ✅ |
| M3 | XLSX/PPTX/ODF + RTF GBK + 内嵌图 OCR 处理链 | ✅ |
| M4 | DjVu（djvu-rs）+ 页内混合路由（intra_page 实验特性）+ RAG 分块 emit | ✅ |
| M5 | FB2 / MOBI（MOBI7）/ CBZ 电子书扩展 | ✅ |
| 增强 | md 解析升级（pulldown-cmark+GFM+frontmatter）、TXT 分节（1.1/一、/（一））、chardetng 编码探测、章节规则可配置 | ✅ |
| 格式扩展轮 | 图像直读（png/jpg/webp/bmp/tiff）、azw3/KF8（自研记录解析）、CHM（libchm+SVN Book 真件金测）、压缩包（zip/rar/7z，rar=纯 Rust rars） | ✅ |

环境限制（OCR 运行时验证需 pdftoppm + 模型缓存，本机金测用 mock 引擎 +
ghostscript shim；详见 docs/开发记录.md）。

## 快速上手

按场景三行依赖声明（文本族随 `default` 自带，见节末说明）：

```toml
# 翻译管线：TXT 中文编码链 + 源语言检测 + docx / epub / pdf 文字层
docforge = { version = "0.1", default-features = false, features = ["encoding", "lang-detect", "docx", "epub", "pdf"] }

# OCR 扫描件：pdf 页级路由 + oar OCR 引擎（ocr-oar 已含 pdf 与 ocr，开箱即用）
docforge = { version = "0.1", features = ["ocr-oar"] }

# 全格式解析转换
docforge = { version = "0.1", features = ["docx", "xlsx", "pptx", "odf", "epub", "fb2", "mobi", "chm", "cbz", "djvu", "pdf", "ocr-oar", "archive", "image-files", "encoding", "lang-detect", "json"] }
```

三个场景的 parse + emit：

```rust
use docforge::{emit, parse, BlockSource};

// 翻译：解析 → 翻译单元（id 即译后回写锚点，chapter 供术语一致性）
let doc = parse("book.epub")?;
for u in emit::to_translation_units(&doc) {
    println!("{}\t{}", u.id, u.text);
}

// OCR 扫描件：pdf 页级混合路由（文字层直提 vs 渲染+OCR），置信度随块携带
let doc = parse("scan.pdf")?;
for b in doc.blocks().filter(|b| b.source == BlockSource::Ocr) {
    println!("[置信度 {:?}] {}", b.confidence, b.content);
}

// 全格式转换：任意件 → JSON / Markdown
let doc = parse("report.docx")?;
println!("{}", emit::to_json_pretty(&doc));
println!("{}", emit::to_markdown(&doc));
```

文本族（txt / md / csv / html / rtf / srt 等字幕）随 `default` feature 自带，无需声明；
报「没有可处理该格式的插件: txt」说明 `default-features` 被裁了，把它加回来即可。

## 许可

MIT OR Apache-2.0。吸收自 docparser 家族与 document-parser 的源码见
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)（源码以 crates.io 存档为准，
随库于 `vendor/*.crate`）。
