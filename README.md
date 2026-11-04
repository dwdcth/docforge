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
| 翻译闭环轮 | 双语回写器七形态（md/docx/epub/txt/html/fb2/odt，定位改写保原格式）、流式消费两层（engine PageSink + pdf 深度流式）、engine/pdf 双级进度钩子 | ✅ |
| 痛点收口轮 | **表格进翻译流**（单元格级单元 + docx/html/epub/odt 格内回写替换）、极简 docx 产行块、块级语言检测（`lang_detect_per_block`）、流式回写会话（`WritebackSession`）、PDF 附录载荷、公开 benchmark（[BENCHMARK.md](BENCHMARK.md)）、非 UTF-8 路径全链路（`AsRef<Path>` + CLI `args_os`） | ✅ |
| rumi 接入轮 | **xlsx 回写器**（格单元 inlineStr 改写 + 译文 sheet 附录，翻译闭环第八格式）、**EPUB 部件交替**（`writeback_epub_alternating`：spine 每部件后插纯译文部件，真·一页原文一页译文）、防幻觉事实谓词（`has_translatable_text`：纯数字/单位/版本号单元不送引擎，策略归消费者）、docx 表格译段小字号（9pt） | ✅ |
| PDF 翻译优化 | **docx 节交替**（`writeback_docx_alternating`：分节符/分页符切段，PageAlt 在 Word 的近似，节属性逐字节保真）、**跨页重复块标记**（`running-detect` 处理链：同文本每页同位置 → `meta["running"]` + header/footer 区域，`TranslationUnit.running` 携带、附录默认跳过——页眉页脚不再翻 N 遍灌满附录） | ✅ |

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

翻译闭环（提取 → 翻译 → 回写，表格按单元格进流，锚点 `p{页}-b{块}-r{行}c{列}`）：

```rust
use docforge::emit::BiStrategy;
use docforge::writeback;

let doc = parse("spec.docx")?;                       // 表格参数表/对照表全部进单元流
let units = emit::to_translation_units(&doc);        // 含格单元（unit.kind == Table、unit.header 标表头）
let translated: Vec<_> = units.iter()
    .map(|u| (u.id.clone(), my_translate(&u.text)))  // 你的翻译引擎
    .collect();
// 定位改写保原格式：样式/边框/非正文部件原样，格内文本换译文
writeback::writeback_docx("spec.docx", &translated, BiStrategy::TranslationOnly, "spec-译.docx")?;
// 超大文档：parse_streaming 逐页投递 + WritebackSession 流式收译文（spill 落盘）
```

文本族（txt / md / csv / html / rtf / srt 等字幕）随 `default` feature 自带，无需声明；
报「没有可处理该格式的插件: txt」说明 `default-features` 被裁了，把它加回来即可。

## Roadmap

- ~~超大文档流式消费~~ ✅ 已落地：`engine::parse_streaming`（全格式逐页投递）+
  `backends::pdf::parse_streaming`（深度流式，页产即吐）+ 回写侧
  `writeback::stream::WritebackSession`（译文流式喂入、spill 落盘）——
  「解析→翻→写」三层全通；吞吐/内存数字见 [BENCHMARK.md](BENCHMARK.md)。
- PDF 双语输出（三路线，按成本递增；"渲染归消费者"的分工不变）：
  **① 附录页（已就绪）**：`writeback::appendix` 出按原文页分组载荷，rumi 用 oxidize-pdf
  渲染追加页——只需"新建页 + 嵌入 CJK 字体子集 + ToUnicode"，能力匹配；
  **② 覆盖式替换 / 双层对照（中间路线）**：原 bbox 内白底遮盖 + 框内断行 + 字号自适应
  （PDFMathTranslate 形态，成本比完整重排低一个量级）。所需数据 IR 尚有缺口：文字层行
  仅 bbox 无 quad、字号/颜色/行高、书写方向、UserUnit/CropBox 换算、遮盖背景采样——
  按 rumi 实做时逐项补；注意 oxidize-pdf 往**已有页面追加内容流**与新建页是两种能力，双层
  方案启动前先核实；
  **③ 完整重排（BabelDOC 形态）= 排版引擎**，不做。
  **边界决定（归属已定，实现待触发）**：溯源数据归 docforge、改写动作归 rumi——"哪些字形属于
  这个块"只在内容流解释器运行时现成（聚合 glyph→line→block 时顺手可得，事后还原贵且两套
  解释器结果可能不一致→擦错块）；删算子/`Tr 3`/重序列化/xref 是写侧能力，与遮盖+叠加同属
  一个渲染流程。**前置问题**：双层对照若目标形态是"原文保留可检索、译文视觉叠加"，则整个
  擦除需求推迟；只有"干净单语译文 PDF"才需要。预留 IR 扩展点（真做时实现）：glyph-run 级
  （非算子级——TJ 数组可跨块）`(stream_id, op_index, 数组元素范围)`；Form XObject 嵌套路径
  + `shared` 标记（共享流擦一页伤多页，克隆或降级遮盖由 rumi 定）；偏移针对解码后流并带
  流指纹；`Option` = 无溯源（OCR 页本无算子，只能遮盖）。出现第二个渲染消费者时渲染层抽
  独立 crate（如 pdf-overlay）。
- 性能队列：① 回写 benchmark 基线 + **A/B 实验**（改写条目 Stored vs Level1 vs 默认——注：我们已是 zlib-rs 后端而非咨询假设的 miniz_oxide，收益以实测为准；内存 Cursor vs 文件 I/O；解析/扫描分项）→ ② 流式 apply_edits（拼接产物不驻留第二份 String）+ 回写 I/O 包（按 A/B 结果采纳压缩等级/内存 Cursor<50MB/大 BufWriter；reflink 尾追加挂远期——CoW 文件系统依赖）→ ③ 超大件 O(段落) 内存重构门槛 = 解压后 document.xml > 200MB 或 WASM/移动端目标。
- crates.io 发布：依赖面已收敛（oar-ocr 移出发布面），待维护者邮箱验证。

## 许可

MIT OR Apache-2.0。吸收自 docparser 家族与 document-parser 的源码见
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)（源码以 crates.io 存档为准，
随库于 `vendor/*.crate`）。
