# 通用文档解析（docforge）规划

> 状态：规划（2026-11-03），未开工。本文是实施前的完整设计蓝图，实施时按 §13
> 的多代理流水线另出逐条规格（`docforge-spec-*.md`），规格与本文冲突以规格为准。
> 前置依赖：翻译主线（`translate-design.md`）收尾后启动，两项目共用工人流水线但文件集隔离。

## 1. 背景与动机

rumi 的 PDF 处理（`ocr/src/pdf_route.rs` 462 行 + `dual_pdf.rs` 619 行）已经解决了
通用文档解析里最难的一环——**页级混合路由**（逐页判断"文字层直提 vs 渲染+OCR"），
但这份资产被锁在 OCR 场景里。与此同时：

- [docparser-sdk](https://crates.io/crates/docparser-sdk) 的统一 IR / Registry /
  BackendPolicy 架构是生态里最好的设计，但它是某 agent 组织的 Python 移植项目，
  **GitHub 仓库已删（404）**，0.2.1（2026-10-28）为终版；其 OCR 子系统从未发布。
- 生态空窗真实存在（调研见 §2）：没有活跃维护的"统一 IR + 页级路由 + OCR"组合。

目标：做一个**独立通用文档解析 crate（暂名 docforge）**——吸收 docparser 的 IR 与
架构骨架，**参考 rumi pdf_route 的页级混合路由设计重新实现**（设计吸收，非代码
迁移，许可理由见 §12）；OCR 与翻译支撑是一等能力。与 rumi-ocr 无耦合、无依赖
方向，成熟后由 rumi-ocr 反向采用（采用路径见 §10）。

> 定位修订（2026-11-03，用户定夺）：初版规划按"rumi 生态成员、M1 切换 rumi-ocr"
> 编写；现改为独立项目——rumi-ocr 是设计参考与后期消费者，不是被重构对象。
> §3/§4/§10/§11/§13/§15 已按此修订。

## 2. 调研结论（2026-11-03，全部实测核实）

### 2.1 docparser-sdk 解剖（源码取自 crates.io 存档，仓库已 404）

- 家族：`docparser-{core,pdf,office,epub,odf}`，共 15.8K SLOC；facade 为 docparser-sdk。
  存档直链：`https://static.crates.io/crates/docparser-<x>/docparser-<x>-0.2.1.crate`
  （国内可走 `https://mirrors.ustc.edu.cn/crates.io/crates/...`）。
- **IR（core/src/model.rs）**：`Document{meta,pages}` → `Page{name,blocks,text,images,w/h/rotation}`
  → `Block{kind, content, bbox, level, ordered, lang, table, meta}`。
  BlockKind 九类：heading/paragraph/list/code/table/image/quote/chart/note。
  亮点：① MetaValue 刻意限 `Bool | Str | List<String>` 三形态，**不把 serde_json::Value
  拉进内核**（裸内核只依赖 serde）；② 全类型 serde 往返，JSON 是线上格式；
  ③ `Document::finalize()` 统一回填统计（页数/字符数/图片数）。
- **插件模型**：两层能力——解析能力（格式绑定，产 `RawDoc`：RawTextItem 带
  level/list/table/chart/image 标记，`assemble` 按"表格>图表>标题>列表>段落"优先级产 Block）
  + 处理能力（全局共享，就地增强 IR，如 OCR 补图像块文本）。
- **路由**：magic 嗅探（OLE/ZIP/PDF 头 + 扩展名兜底，OOXML 头是 OLE 判加密件）+
  `BackendPolicy` 回退链（主后端失败自动降级；encrypted/unsupported 为终止码不回退）。
- **PDF 后端**：pdfium-render（PDFium 动态库）+ 独占 worker 线程（Pdfium 非 Sync）；
  `FPDFText_CountRects/GetBoundedText` 逐行抽取**含 Form XObject 文本**；CJK CID 靠
  PDFium 内建 CMap；兜底 pdf-extract（纯 Rust）。
- **Office 后端**：zip + quick-xml 流式（纯 Rust），docx.rs 1641 行含编号/表格/图片位置还原。
- **OCR**：缺席（其文档提及的 docparser-ocr crate 不存在）。
- 许可：MIT OR Apache-2.0。

### 2.2 竞品成色

| 库 | 状态 | 评估 |
| --- | --- | --- |
| document-parser | 0.1.0-alpha.18（2026-09-29） | 活跃 alpha；章级 HTML IR（非块级）。**已解剖并定向吸收**（§2.4：TXT 分章算法骨架、EPUB 资源五件套、FB2、DjVu 页级分治模式、whatlang、安全实践）；其 DOCX 因中文内容丢失级缺陷不采 |
| ebook-rs | 0.18.0（2026-10-27），61 版 | 活跃但方向是电子书引擎（CFI/LCP/TTS/RAG 分块），月下载 ~151；非同类 |
| deformat 0.16 / anydoc 0.2.4 | 存在 | 轻量文本提取，无 IR/路由设计 |
| djvu-rs 0.44 | 活跃 | DjVu 候选其一（另一为 `djvu 0.1.0`，见 §2.4-4），M4 实测定选型 |
| oxidize-pdf 5.5.6 | 活跃（rumi 已用于写侧） | 继续用于双层 PDF 导出 |
| pdfboss / parser-core | 404 / yanked | 幻觉/死条目，不采信 |

### 2.3 rumi 侧事实（定位：设计参考，非代码迁移源）

- `pdf_route.rs`：pdf-inspector（`ScanStrategy::Full` 逐页）分类 → 文字层页
  `group_into_lines_preserving_all_text` 直提（行+box，**PDF 点坐标系、左下原点**，
  见该文件头注释的坐标系契约）→ 扫描页 pdftoppm（系统 poppler）区间合并渲染 + oar-ocr。
- `dual_pdf.rs`：IR 消费者样本（页面原图 + 隐形文字层 + CJK 字体子集化，oxidize-pdf）。
- **pdf-inspector 1.25.2 的重依赖全部 feature 门控（M0 已核实，docs/开发记录.md）**：
  `default = []`，firecrawl-pdfium（render-pdfium）/oar-ocr+ort（ocr-oar）/pyo3
  均非默认；文字层分类与提取在 default 纯 Rust 面内。"纯 Rust 路线"与 PDFium
  之争实际不存在——docforge 只用其 lopdf 面做分类+文字层（渲染走系统 pdftoppm，
  OCR 走 docforge 自有 ocr feature）。

### 2.4 document-parser（B）解剖与吸收裁决（2026-11-03，源码已 vendor）

源码 vendor 于 `vendor/document-parser-0.1.0-alpha.18/`（crates.io 存档；上游
LiiChar/document-parse 活跃）。对比研究范围：B 的 formats/ 与 utils/ 逐格式对照
docparser（A）对应实现，结论带行号记录于本节。

- **IR 不采**：章级 `Document{metadata, chapters: Vec<Chapter{title, Text|Html}>}`，
  面向阅读器的归一化 HTML/文本，无块级结构——与 A 的九类 Block 不同量级，
  印证"以 docparser 为主干"。
- **DOCX 后端不采（中文致命缺陷）**：`is_chapter_title` = 词数 ≤12 启发式
  （loader.rs L215/L654-667），中文无空格 → `split_whitespace().count()`=1 →
  **任意中文段落被判为章标题且正文被 `continue` 丢弃**；标题仅认 `Heading`
  前缀 styleId（zh-CN Word 数字样式全漏）；单元格多段各产一个 `<td>` 列数错位。
- **吸收清单**（落点见 §7 表）：
  1. TXT 分章算法骨架（五类候选强弱类型 + 弱类型需相邻同类确认 + 首边界
     出现在正文 60% 后视为误判，text.rs L45-220）——词表仅 RU/EN，对中文现状
     是"永不分章"，重写词表加"第X章/回/节/卷/幕、序章、楔子、番外"；
  2. EPUB 资源五件套：manifest 图片字节提取、章内 `img src`→资源 id 统一重写
     （相对路径/percent-decode/SVG `<image>` 包装四形态）、三级封面 fallback、
     spine `linear="no"` 跳过、XML 声明编码嗅探（前 1KB）；
  3. FB2 loader（453 行，roxmltree，产 HTML）——B 独有格式中性价比最高，M5；
  4. DjVu 页级分治模式（文字页 `page.text()` 直提 / 无字页 `render_to_size()`
     渲染）——与 rumi 页级路由同构，无字页接 oar-ocr 即差异化位；但其依赖
     `djvu 0.1.0`（bugzmanov，2026-03 后未更）弱于 `djvu-rs 0.44`
     （matyushkin，活跃，读/渲染/转换/CLI），M4 以 text() 能力实测定选型；
  5. whatlang 语言检测限流采样（前 3 章截 1 万字符，language.rs）→ docforge
     处理能力 `lang-detect`（§8.4），rumi 翻译主线消费源语言；
  6. 安全实践：资源落盘文件名 = sha256(resource_id) 防路径穿越（parser.rs
     L294-309）；资源 base64 内联 / 落盘双模式；
  7. `normalize_pdf_text` 的行尾连字符断词合并（pdf/loader.rs L103-105）——
     PDF 文本后处理小件，任何 PDF 后端可复用；
  8. feature 组织：按格式分 feature + documents/ebook/full 聚合——docforge
     发布面控制参照（亦为 Q3 pdf-inspector 重依赖可选化的先例）。
- **明确不引入**：html2text（80 列折行后 `split_whitespace` 丢全部段落结构，
  只配喂语言检测）；ammonia（透传不可信 HTML 场景的措施，块级 IR 解析重建
  输出无透传面）；rtf-parser 0.4（依赖 wasm-bindgen，年轻）；scraper 0.27
  （html5ever 全家桶，FB2/MOBI 接入时改喂自有 HTML 扫描器）；docx-rust（见上）。
- 修正一处 §2.1 旧记录：docparser-core 的 `plugins/html.rs` 是**零依赖手写
  字节扫描器**（非 quick-xml），吸收时需修补三点：属性值含 `>` 时标签截断、
  无 `<img>`→Image 块（配 EPUB 五件套正好闭环）、`li` 丢弃 ol 有序信息。

## 3. 目标与非目标

**目标**
1. 统一 IR 的多格式解析：PDF（页级混合路由 + OCR）、DOCX、EPUB、TXT/MD/CSV、
   HTML 为 v1；XLSX/PPTX/ODF、RTF、DjVu 分期；FB2/MOBI/CBZ 视需求（M5）。
2. **独立项目**：不依赖 rumi-ocr，OCR（oar-ocr 接入）与翻译支撑
   （lang-detect、翻译友好 emit / 双语对齐）为一等能力；`adapt::ocr_lines`
   仅为 rumi 后期采用预留接口位，不在主线里程碑内。
3. 独立 crate，可单独使用、可发布 crates.io（名字已核可用，见 §11）。
4. 输出：结构化 IR（serde JSON）+ Markdown emit；OCR 置信度进 IR。

**非目标（明确不做）**
- 渲染引擎 / 电子书运行时（CFI/LCP/TTS）——ebook-rs 领域。
- DOC/PPT/XLS 老格式（OLE）——转投 LibreOffice 桥接的需求留待用户反馈。
- 页内混合路由（一页内文字层块与扫描图区共存分治）——v2 之后（§8.4）。
- 写侧（生成 DOCX 等）；PDF 写侧继续走 oxidize-pdf，本 crate 只读。

## 4. 总体架构

```
┌─ 任意消费者（翻译流水线 / RAG / rumi-ocr【后期采用】/ …）────────────┐
│         可选：rumi 采用时的 OcrLine 适配层（docforge::adapt）        │
├─ docforge（独立 crate：lib + bin docforge-cli）─────────────────────┤
│ lib.rs     parse(path, Options) -> Document（统一入口）            │
│ model.rs   IR：Document/Page/Block/…（§5）                        │
│ router.rs  格式级路由：magic 嗅探 + BackendPolicy 回退链           │
│ registry.rs 能力注册（解析能力 + 处理能力两层，沿 docparser 设计） │
│ backends/  pdf.rs（页级路由，参考 rumi pdf_route 设计重实现        │
│            + oar-ocr OCR 接入；M1）                                │
│            docx.rs / epub.rs / text.rs（docparser 源码吸收改造）   │
│            xlsx.rs / pptx.rs / odf.rs（三期，docparser-office 迁） │
│            djvu.rs（四期）/ fb2.rs mobi.rs cbz.rs（五期）           │
│ emit.rs    Markdown / JSON / 翻译友好（段落对齐）输出               │
│ adapt/     ocr_lines.rs（rumi 采用预留，后期，不在主线里程碑）      │
└────────────────────────────────────────────────────────────────────┘
```

- 独立 workspace（`/mnt/s790/project/docforge/`，M0 起 git 仓库）；与 rumi-ocr
  无依赖方向，rumi 成熟后以 crates.io 或 git 依赖反向采用。
- 全同步 API（与 docparser 一致；异步包装留给消费者）。

## 5. IR 设计（基于 docparser model.rs，四处修改）

采纳其全部成熟设计（九类 BlockKind / MetaValue 三形态 / serde 往返 / finalize 统计），
**修改/扩展点**：

1. `Block` 增加 `confidence: Option<f32>`——OCR 路径的行置信度（rumi OcrLine 已有 score，
   翻译/RAG/双层 PDF 消费者都要用；docparser 缺此字段）。
2. `Block` 增加 `source: BlockSource`（`#[serde(default)]`，wire 兼容）：
   `TextLayer`（PDF 文字层直提）/ `Ocr`（渲染+识别）/ `Native`（DOCX/EPUB 等原生结构）。
   消费者据此区分"零失真文本"与"识别文本"（现有 server 的快路径语义平移）。
3. `Page` 增加 `coord_units: Option<CoordUnits>`（`Pt`/`Px(dpi)`/`Logical`）：docparser
   "坐标语义由产生方决定"对消费者不友好；先显式标注单位，长期统一归一化坐标
   （见开放问题 Q1）。PDF 文字层 Pt（左下原点契约随 pdf_route 头注释整体迁入文档），
   OCR 页 Px（渲染 DPI 已知）。
4. `DocMeta.extra` 沿用；`file_type` 用 router 的规范格式名。

RawTextItem / assemble / 两层能力模型 / Registry / BackendPolicy **原样吸收**（改动最小化，
便于与上游语义对表——它的 docx/epub 后端产 RawDoc 的逻辑可以整块搬）。

## 6. 路由设计

两层，各取所长：

1. **格式级**（docparser router 吸收）：magic（OLE/ZIP/PDF/`{\rtf`）+ 扩展名兜底 →
   规范格式名；BackendPolicy 链（如 pdf 主后端 pdf-inspector → 失败降级 pdf-extract；
   encrypted/unsupported 终止码不回退）。
2. **页级**（rumi 资产，docparser 没有）：仅 PDF 后端内部——pdf-inspector 逐页分类，
   文字层页直提（Block source=TextLayer），扫描页 pdftoppm 区间渲染 + oar-ocr
   （Block source=Ocr + confidence）。混合文档逐页走最优路径。

## 7. 后端选型与来源

| 格式 | 后端实现 | 来源 | 许可 | 期 |
| --- | --- | --- | --- | --- |
| PDF | pdf-inspector（分类+文字层）+ pdftoppm 渲染 + oar-ocr（扫描页）；文本后处理吸收断词合并小件 | rumi pdf_route 迁移 + B pdf/loader.rs L103-105 | 现有 | M1 |
| TXT/MD/CSV | A plugins/{text,markdown,csv} 原样 + **自写编码链**（UTF-8 → UTF-16 BOM → GB18030/Big5（encoding_rs 错误计数判别）→ lossy；两侧现状都不支持 GBK/Big5）+ **B 分章骨架**（中文词表重写） | A 吸收 + B 改造 + 新写 | A/B 均 MIT/Apache-2.0 | M2 |
| DOCX | A docx.rs 整块搬（quick-xml 流式；补 styles.xml 样式名→层级映射，覆盖 zh-CN Word 数字 styleId；可抄 B 超链接 rId 解析与 docProps 元数据两小段） | A 吸收 + 小补 | 同上 | M2 |
| EPUB | A epub lib.rs 主干（nav/NCX 三级标题、多作者元数据、块级分章）+ **B 五件套融合**（§2.4-2） | A+B 融合 | 同上 | M2 |
| HTML | A plugins/html.rs（§2.4 三修补：属性引号 `>`、`<img>`→Image、ol 有序）——EPUB 融合的前置依赖，独立 .html 解析顺带开放 | A 吸收 + 修补 | 同上 | M2 |
| XLSX/PPTX/ODF | docparser-office/odf 全量吸收 | 同上 | 同上 | M3 |
| RTF | A plugins/rtf.rs（\uN 代理对/`\'hh` 查表状态机）+ 补 GBK/Big5 代码页表（encoding_rs）替代 hard error | A 吸收 + 小补 | 同上 | M3 |
| DjVu | `djvu-rs 0.44`（matyushkin，活跃）与 `djvu 0.1.0`（bugzmanov）M4 实测 text() 能力二选一；页级分治按 B 模式 + 无字页接 oar-ocr | 新胶合 + B 模式 | 随所选 crate | M4 |
| FB2/MOBI/CBZ | B fb2 loader（roxmltree，HTML 产出喂自有 HTML 扫描器）；MOBI 待 mobi crate 语料验证；CBZ 自写 ~100 行（natord 排序） | B 吸收/参考 + 自写 | B MIT/Apache-2.0 | M5（视翻译用户需求） |

## 8. OCR 集成与处理能力链

1. oar-ocr 以**处理能力**（docparser 的 process plugin 语义）接入：PDF 扫描页在
   pdf 后端内直接产 Block（快路径，页级路由的既有形态）。
2. 对 DOCX/EPUB 内嵌图片的 OCR（可选 `ocr-images` Options 开关）：解析产出
   `Image{data,bbox}`，处理能力链补 `ocr_text`——docparser IR 已留好此位
   （Image.ocr_text 字段），我们的差异化正是"它设计了槽位但没有引擎，我们有"。
3. 置信度过滤阈值进 Options（低置信度行标记而非丢弃，meta.truncated 类似语义）。
4. `lang-detect` 处理能力（B 思路，whatlang + 限流采样）：产出
   `DocMeta.extra["language"]` 并回填 Block.lang——rumi 翻译主线的源语言
   检测直接消费，检测失败不致命（处理链语义，仅记 warnings）。

## 9. 输出层

- `emit.rs`：`to_markdown(&Document) -> String`（heading 层级/表格 MD 表格/代码围栏/
  图片占位 `[image]`）、`to_json`（serde 直出）、**翻译友好 emit**（段落级原文输出
  带 block 序号/页号/lang，供翻译管线对齐回写双语）。docparser emit 思路 + 表格
  管道符对齐。
- `adapt::ocr_lines(&Document) -> Vec<OcrLine>`：rumi 采用时的适配层（含坐标系
  契约平移、四点框↔Rect 转换）——**预留接口位，rumi 采用期实现，不在主线里程碑**。

## 10. 与 rumi-ocr 的关系（独立开发，后期采用）

1. **独立开发**：docforge 自建 git 仓库、自跑测试，不引用 rumi 代码路径；
   rumi 的 `pdf_route.rs` / `dual_pdf.rs` / `OcrLine` 是**设计参考**（页级路由
   流程、坐标系契约、双层 PDF 消费形态），在 docforge 内重新实现（§12 许可）。
2. **rumi 采用（后期，独立排期，不在 M0–M5 内）**：rumi-ocr 增加 docforge 依赖，
   `pdf_route.rs` 变薄壳调 `docforge::adapt::ocr_lines`；验收回归 rumi 基线
   （`cargo test -p rumi-ui -p rumi-ocr` 60+1 / 30+2 + demo 插件 4 个不回归）、
   server `/api/ocr` PDF 分支错误文案逐字保留（http_tests 有据此断言的用例）、
   双层 PDF 端到端记录（docs/开发记录.md）重跑。
3. **对表纪律**：docforge 的 PDF 快路径在 M1 验收时与 rumi pdf_route 的输出做
   一次质量对表（同文件、行数/文本相似度），差异记录进开发记录——这是"设计
   等价"的证据，也是 rumi 采用时的信心基础。

## 11. 命名与发布

- crate 名 **docforge**（`docforge.crate` 于 crates.io 已核可用；备选 docroute/pagedoc/
  rumi-docparse 亦可用，2026-11-03 查）。lib 名 `docforge`，bin `docforge-cli`
  （parse→md/json 输出，`--pages 1-20` / `--ocr on|off|auto` / `--format ir|md|text`）。
- 发布策略：**独立仓库起步**（git init 于 `/mnt/s790/project/docforge/`，vendor/
  随库以保吸收源可追溯）；M2 后视成熟度发 crates.io（发布前按 translate
  项目的经验补 description/repository、docs.rs 元数据、`cargo package --list` 检查）。

## 12. 许可证合规

- docparser 家族与 document-parser 均为 MIT OR Apache-2.0：吸收其源码
  （model/router/registry/office/epub/odf/plugins + document-parser 的
  text.rs 分章/epub 资源五件套/fb2/djvu 模式）须在吸收文件保留原版权与
  许可声明（建 `docforge/THIRD_PARTY_NOTICES.md` 集中列出来源 crate、版本
  （docparser 家族 0.2.1 / document-parser 0.1.0-alpha.18）、存档 URL）。
  源码以 crates.io 存档 tarball 为准（两家 GitHub 前者已删、后者非发布依据，
  存档是唯一合法源，§2.1 有直链；两家均已 vendor 于 `docforge/vendor/`）。
- docforge 自身许可 **MIT OR Apache-2.0**（与被吸收代码同构；独立项目无 MulanPSL
  混搭问题，rumi 采用 MIT/Apache 依赖无合规障碍）。
- **rumi-ocr（MulanPSL-2.0）代码不复制**：pdf_route/dual_pdf 仅作设计参考在
  docforge 重新实现（思想不受版权约束，表述/结构独立写）；若未来确需移植其
  代码段，按 MulanPSL 署名条款单独评估并记入 THIRD_PARTY_NOTICES。
- pdf-inspector / oar-ocr / oxidize-pdf / djvu-rs 按各自许可正常依赖，发布时随
  cargo metadata 自动携带。

## 13. 里程碑与验收

| 期 | 内容 | 验收标准 | 预估规模 |
| --- | --- | --- | --- |
| M0 | 初始化 crate；vendor docparser-{core,office,epub} 0.2.1 与 document-parser 0.1.0-alpha.18 源码与声明；IR 定稿（§5 四处修改） | `cargo check` 过；IR 单测（serde 往返/confidence/source/coord_units）绿 | 0.5K SLOC |
| M1 | pdf 后端（页级路由，参考 rumi pdf_route 设计**重新实现**，含坐标系契约注释）+ router/registry 接线 + OCR 引擎接入（oar-ocr 处理能力，扫描页产 Block） | 混合 PDF（图封面+文字正文）逐页路由金测绿；扫描 PDF OCR 金测（confidence/source 进 IR）；与 rumi 快路径同文件质量对表并记录差异 | 1.5K |
| M2 | 文本族（TXT 编码链 + B 分章中文词表改造 / MD / CSV）+ DOCX + EPUB（B 五件套融合，含 HTML 扫描器三修补）+ emit（md/json）+ lang-detect 处理能力 + docforge-cli 雏形 | 每格式≥3 个真实文件金测（含中文 DOCX/EPUB/**GBK 与 Big5 TXT、中文章回分章**）；md 输出人工抽查；CLI 冒烟 | 3.2K（含吸收） |
| M3 | XLSX/PPTX/ODF + RTF（补 GBK 代码页表）+ 图像 OCR 处理能力链（ocr-images 开关） | docparser-office 原测试集平移通过 + 新增用例；内嵌图片 OCR 金测；中文 RTF 金测 | 2.3K |
| M4 | DjVu（双 crate 实测定选型，无字页接 OCR）+ 页内混合路由（文字层块与图区共存分治）+ RAG 分块 emit | DjVu 扫描书金测（文字页直提 + 无字页 OCR）；页内混合 PDF 构造件金测 | 1.5K |
| M5 | FB2 / MOBI / CBZ 电子书扩展（B 吸收 + 自写，视 rumi 翻译用户需求启动） | FB2 金测（含 fb2.zip）；MOBI 语料成功率报告；CBZ 页序金测 | 1K |

执行方式沿用 `docs/开发经验.md` §六 流水线：每期先出 `docforge-spec-*.md`
（API/用例/文案写死）→ 文件集不相交的工人并行 → 门禁（cargo check/test + 审 diff +
歧义点清单核对 + 修复轮）。

## 14. 风险与开放问题

**风险**
1. 工作量：总盘子 ~8K SLOC（含吸收），是翻译项目量级；严格分期，每期独立可交付。
2. ~~pdf-inspector 依赖面重~~ **已解除（M0 核实）**：`default = []`，重依赖全
   feature 门控，文字层路径纯 Rust（见 §2.3 与 docs/开发记录.md Q3 三问）。
3. document-parser 若修复中文缺陷（§2.4 DOCX 段）并转正，吸收面可再对表减负
   （每年复查一次生态）；FB2/MOBI 的上游演进同理。
4. pdftoppm 渲染依赖系统 poppler（rumi 现状即如此），独立发布时在 README 标注系统依赖。

**开放问题（实施前定夺）**
- Q1 坐标：显式单位标注（本规划方案）vs 全面归一化 0–1 坐标（消费者更省心但
  PDF 点→归一化需要页框高度，pdf-inspector 不暴露——回到 lopdf 直读问题）。
  推荐 M1 用显式标注，归一化列为 M4 重构项。
- Q2 EPUB 的 XHTML→Block 规则深度（仅 h/p/li/pre/table 还是含 img/figcaption）——
  M2 规格阶段定。
- Q3 pdf-inspector 重依赖是否可选（影响 §12 发布面），M0 第一件事核实。
- Q4 XLSX 公式单元格取值（缓存值 vs 不求值）——M3 规格，预期取缓存值并在 meta 标注。

## 15. 与现有项目的关系

- **独立项目**（2026-11-03 用户定夺）：docforge 与 rumi-ocr 无依赖关系，
  rumi 是设计参考（§2.3）与后期消费者（§10）；吸收 rumi 设计须重新实现
  （§12 许可），不复制其源码。
- 翻译主线（translate-design.md）与 docforge 并行不阻塞；docforge 的
  lang-detect / 翻译友好 emit 是翻译主线的上游供给，采用节奏由翻译侧定。
- 本规划落地时更新 `docs/task-queue.md` 挂接期次（docforge 自建 task 文件，
  rumi 侧仅在采用期挂接）。
