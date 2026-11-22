# docforge

通用文档解析 crate 与 CLI：docparser 式统一块级 IR + 格式路由（有界 magic 嗅探 +
后端回退链）+ **PDF 页级混合路由**（逐页判定"文字层直提 vs 渲染+OCR"，设计参考
rumi-ocr 的 pdf_route）。OCR 置信度与块来源进 IR；输出 JSON / Markdown / GFM /
Google OKF v0.3（含图片资产与目录索引）/ 翻译友好分段，服务解析转换、知识库构建、
OCR、翻译四类消费者。独立项目，与 rumi-ocr 无依赖关系。

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
| 知识库与老式 Office 轮 | **统一资源安全护栏**（`limits` 防 ZIP/XML 炸弹 + `router::sniff` 有界头探测与 OLE/ZIP 内容嗅探 + 确定性变异鲁棒测试）、**数学公式转 LaTeX**（零依赖 `math` 模块，OMML/MathML → `$...$`/`$$...$$`，覆盖 docx/odt/html/epub；docx `styles.xml` `basedOn`/`outlineLvl` 继承链）、**GFM & Google OKF v0.3 输出**（`emit::to_gfm` + `okf` 模块，含 YAML 1.2 frontmatter / 图片去重落盘 `assets/` / 递归 `index.md`）、**老式二进制 Office**（吸收 `firecrawl/anydoc` 0.2.5：`doc`/`ppt`/`xls`/`xlsb` 4 细粒度 + `legacy-office` 聚合）、**7 平台 CLI CI 发布** | ✅ |

环境限制（OCR 运行时验证需 pdftoppm + 模型缓存，本机金测用 mock 引擎 +
ghostscript shim；详见 docs/开发记录.md）。

## 快速上手

按场景依赖声明（文本族随 `default` 自带，见节末说明）：

```toml
# 翻译管线：TXT 中文编码链 + 源语言检测 + docx / epub / pdf 文字层
docforge = { version = "0.1", default-features = false, features = ["encoding", "lang-detect", "docx", "epub", "pdf"] }

# 老式 Office 按需开启（4 细粒度 doc / ppt / xls / xlsb，或一键 legacy-office）
docforge = { version = "0.1", features = ["legacy-office"] }

# 全格式解析转换（纯 Rust 零 C 编译依赖）
docforge = { version = "0.1", features = ["docx", "xlsx", "pptx", "legacy-office", "odf", "epub", "fb2", "mobi", "chm", "cbz", "djvu", "pdf", "archive", "image-files", "encoding", "lang-detect", "json"] }
```

典型场景的 parse + emit：

```rust
use docforge::{emit, okf, parse, BlockSource};

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

// 全格式转换：任意件 → JSON / Markdown / GFM / Google OKF v0.3
let doc = parse("report.docx")?;
println!("{}", emit::to_json_pretty(&doc));
println!("{}", emit::to_gfm(&doc));
let okf_concept = okf::to_okf_concept(&doc, &okf::OkfOptions::default());
println!("{}", okf_concept.markdown);
```

## CLI 用法（`docforge-cli`）

`docforge-cli` 默认通过 `cli` feature 开启全部纯 Rust 格式后端与**双轻量化纯 Rust OCR 引擎**（PP-OCRv6 `qppocr` + Indra OCR `fusor-ai`，零 C/C++ 编译依赖），专注文档格式转换，仅输出 **`gfm`**（默认）或 **`okf`**（Google Open Knowledge Format v0.3）；Agent 技能说明见 [skills/docforge/SKILL.md](skills/docforge/SKILL.md)：

```bash
# 从源码构建全格式 + 双纯 Rust OCR CLI
cargo build --release --features cli --bin docforge-cli

# 查看已支持的全部输入格式与 OCR 引擎
docforge-cli --list-formats

# 转为标准 GitHub-Flavored Markdown（默认格式，含 OMML/MathML → LaTeX 公式）
docforge-cli report.docx -o report.md

# 扫描件/图片 OCR：默认使用 PP-OCRv6（自动探测 IP，中国大陆自动走 ModelScope/hf-mirror 镜像加速）
docforge-cli scan.png -o scan.md
docforge-cli scan.pdf --ocr ppocr-tiny -o scan.md     # 极轻 ~6.3 MB 模型

# 切换 Indra OCR（自带 ±45° 纠偏、朝向检测、版面阅读顺序与数学公式转 LaTeX）
docforge-cli paper_scan.png --ocr indra -o paper.md

# 离线指定模型目录或手动指定镜像
docforge-cli scan.png --ocr ppocr --ocr-model-dir ./my_models
docforge-cli scan.png --ocr indra --ocr-mirror cn

# 转为 Google Open Knowledge Format (OKF v0.3)：
# 生成带 YAML 1.2 frontmatter 的文档 + assets/ 去重图片资产 + 根目录 index.md
docforge-cli report.docx --format okf --out-dir ./kb

# 批量目录转换（保持目录树并递归生成各级 index.md 成为完整 Knowledge Bundle）
docforge-cli ./docs_dir --format okf --okf-type Reference --out-dir ./kb
```

GitHub Releases 提供 7 个平台的预编译静态/原生 CLI 包（见 `.github/workflows/release.yml`）：
- Windows: `x86_64-pc-windows-msvc`
- macOS: `x86_64-apple-darwin`, `aarch64-apple-darwin`
- Linux: `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`, `loongarch64-unknown-linux-gnu`

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
- 性能（已完成：raw_copy 直拷未触碰条目、流式 apply_edits + 64KB 批冲、输入内存 Cursor、
  A/B 压缩定案——Level1 省 18% 但体积 +10-20% 不采纳；对照数字与方法学见 BENCHMARK.md）。
  挂账：① 超大件 O(段落) 内存重构，门槛 = 解压后 document.xml > 200MB 或 WASM/移动端
  目标；② reflink 尾追加重打包（克隆 + 截断 Central Directory + 尾部追加新条目，恒定
  2-5ms——CoW 文件系统依赖 APFS/Btrfs/XFS，远期）。
- 优化储备（有空继续，按触发条件拾取）：
  - docx 写入保真：样式感知字号（解析 styles.xml 继承链替代表格格内固定 9pt）；嵌套表格
    格写入（v1 占位不写——清空留展平残留）；文本框内文翻译 opt-in（段落帧栈 + p_depth
    设计已备，rumi 用户反馈"流程图内文字未翻"时启动）。
  - 提取侧形态：epub 单节坍缩页的表格不产 Table 块/格单元（坍缩语义与表格翻译的取舍，
    有真实单节带表 epub 场景再议）；odt/pptx 修订与域文本免疫审计（docx 经 w:t 限定天然
    免疫 instrText/delText，odt tracked-changes 与 pptx 待同款核实）。
  - xlsx：附录模式数值/日期格译文排除改为记 WritebackReport（现静默，W3 交付时留的口径）。
  - 小众格式回写器（mobi/azw3、djvu、chm）："有真实场景再加"——PalmDOC 记录改写量级
    独立立项。
- crates.io 发布：依赖面已收敛（oar-ocr 移出发布面），待维护者邮箱验证。

## 许可

MIT OR Apache-2.0。吸收自 docparser 家族、document-parser 与 firecrawl/anydoc（0.2.5，MIT）的源码见
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
