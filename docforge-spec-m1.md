# docforge M1 规格：PDF 后端（页级混合路由 + OCR 引擎抽象）

> 依据：docparse-plan.md §13（M1 行）+ §6（路由设计）+ §8（OCR 集成）。
> 设计参考：rumi-ocr `ocr/src/pdf_route.rs`（页级路由流程、坐标系契约、
> pdftoppm 区间渲染、样例生成与相似度断言策略）——**重新实现，不复制源码**
>（计划 §12）。
> 状态：随 /loop 自主推进（用户已授权），门禁后 commit。

## 1. 范围

1. `ocr` 模块：`OcrEngine` trait + `RecognizedLine`（引擎可注入，oar-ocr 为
   默认实现——rumi 采用时可注入自有引擎实例，测试可注入 mock）；
2. `backends/pdf`：页级混合路由（Auto/On/Off 三模式），文字层直提
   （TextLayer 块，Pt 坐标）与扫描页渲染+OCR（Ocr 块，Px 坐标 + confidence）；
3. `plugin.rs` IR 管道扩展：`RawTextItem{source, confidence}`、
   `RawPage.coord_units`（透传至 Block/Page）；
4. 顶层便捷 API：`docforge::parse(path)`（默认装配，M1 起可用）；
5. 金测：文字层 PDF + 混合 PDF（oxidize-pdf 生成样例，参考 rumi sample 策略）。

**不做**：emit 扩展与 cli（M2）、内嵌图片 OCR 链（M3）、页内混合路由（M4）、
加密 PDF 处理（pdf-inspector 报错透传即可）。

## 2. feature 布局（Cargo.toml 增量）

```toml
[features]
default = ["md", "csv", "html", "rtf"]   # 不变：裸内核不含 pdf/ocr
pdf = ["dep:pdf-inspector", "dep:image"]   # 页级路由 + pdftoppm 渲染（无 OCR 引擎）
ocr = ["dep:image"]                        # OcrEngine trait + 类型
ocr-oar = ["ocr", "pdf", "dep:oar-ocr"]    # oar-ocr 默认引擎（重依赖隔离于此）
```

```toml
[dependencies.pdf-inspector]
version = "1"
default-features = false     # Q3 结论：default=[]，绝不拉 firecrawl-pdfium/ort/pyo3
optional = true

[dependencies.image]
version = "0.25"
optional = true

[dependencies.oar-ocr]
git = "https://github.com/GreatV/oar-ocr"
features = ["auto-download"]
optional = true

[dev-dependencies]
serde_json = "1"
oxidize-pdf = "5"            # 金测样例生成（写侧，与 rumi 同款用法）
```

运行时依赖纪律检查：default 构建 = 仅 serde（不变）。

## 3. ocr 模块（src/ocr/mod.rs，feature = "ocr"）

```rust
/// OCR 识别行（引擎输出；四点框保真旋转/倾斜，像素坐标、左上原点）。
pub struct RecognizedLine {
    pub text: String,
    /// 识别置信度 0..1
    pub score: f32,
    /// 四点框，顺时针自左上：[[x,y];4]
    pub quad: [[f32; 2]; 4],
}

impl RecognizedLine {
    /// 四点框的轴对齐包围盒（Block.bbox 用）。
    pub fn axis_rect(&self) -> Rect { /* min/max 写 */ }
}

/// OCR 引擎抽象：整页图像 → 识别行。
/// 注入式设计：rumi/上层可注入自有引擎实例（避免模型双重加载），测试可注入 mock。
pub trait OcrEngine: Send + Sync {
    fn name(&self) -> &str;
    fn recognize(&self, image: &image::RgbaImage)
        -> std::result::Result<Vec<RecognizedLine>, String>;
}
```

`src/ocr/oar.rs`（feature = "ocr-oar"）：`OarOcrEngine`——oar-ocr 文字管线
（det+rec，与 rumi 同款 small 模型）的懒加载包装；`recognize` 内
RGBA→RGB 转换后 predict，结果映射 `RecognizedLine`（box→quad、rec_score→score）。

## 4. plugin.rs 管道扩展（全 feature 可见，无新依赖）

```rust
pub struct RawTextItem {
    // …原字段不变…
    /// 块来源（pdf 页级路由标 TextLayer/Ocr；其余格式缺省 Native）
    pub source: BlockSource,
    /// OCR 置信度（source = Ocr 时 Some）
    pub confidence: Option<f32>,
}

pub struct RawPage {
    // …原字段不变…
    /// 本页 bbox 坐标单位（pdf 文字层页 Pt / OCR 页 Px{dpi}）
    pub coord_units: Option<CoordUnits>,
}
```

`BlockSource`/`CoordUnits` 从 model 导入。assemble 路径（engine → 各插件）
透传三字段；既有格式插件不受影响（Default 缺省 Native/None）。

## 5. pdf 后端（src/backends/pdf.rs，feature = "pdf"）

### 5.1 模块头（坐标系契约，随 rumi pdf_route 头注释语义重写）

> pdf-inspector TextItem 坐标 = 可见页框（CropBox∩MediaBox）内、左下原点、
> y 向上、PDF 点（1/72"）；y 对水平文本是基线。公开 API 不暴露页框高度，
> 无法换算左上原点——故文字层页 `coord_units = Pt`（保持原坐标系），
> OCR 页 `coord_units = Px{dpi}`（渲染图像坐标）。同一文档两坐标系并存，
> 消费者按 `Page.coord_units` 区分（归一化列为远期项，计划 §14 Q1）。

### 5.2 公共 API

```rust
/// PDF 页级路由模式。
pub enum OcrMode {
    /// 逐页判定：文字层页直提，扫描页渲染+OCR（默认，rumi 同款）
    Auto,
    /// 全部页渲染 + OCR（强制慢路径，等价 rumi --force-raster）
    On,
    /// 仅文字层；扫描页产出空块 + meta["skipped_ocr"]=true
    Off,
}

pub struct PdfOptions {
    pub mode: OcrMode,                  // 缺省 Auto
    pub dpi: u32,                       // 渲染 DPI，缺省 200（rumi 同款）
    pub engine: Option<std::sync::Arc<dyn OcrEngine>>,  // Auto/On 需要时必须提供
}

pub fn register(registry: &mut Registry, opts: PdfOptions);
```

`ParseCapability`（id="pdf-inspector"，formats=["pdf"]）流程：

1. 预检：`!path.is_file()` → `DocError{Unsupported?}`——用 `Corrupt` 不对、
   rumi 文案是"文件不存在：…"；docforge 统一 `Unsupported("文件不存在：{path}")`
   （终止码，不回退，防兜底后端掩盖）；
2. Auto：`detect_pdf_type_with_config(Full)` → `pages_needing_ocr` 集合；
   Off：raster 集合空；On：全页 raster；
3. 文字层组：`extract_text_with_positions_pages(path, Some(&text_pages))` →
   `group_into_lines_preserving_all_text` → 每行：
   - 跳过空文本 item 与 `ItemType::Image`（不污染行框，rumi 同策略）；
   - bbox = 成员 item 框并集 `Rect`；空行（无有效 item）丢弃；
   - `RawTextItem{text, bbox, source: TextLayer}`（**confidence = None**：
     文字层的语义是"零识别"，1.0 留给 OcrLine 适配层补）；
   - 页 `coord_units = Some(Pt)`；
4. raster 组（On/Auto）：连续区间合并，每区间 `pdftoppm -png -r {dpi}
   -f {first} -l {last}`（临时目录、页码数字排序装载、NotFound →
   `"找不到 pdftoppm（需安装 poppler-utils）"`）→ 每页 `engine.recognize` →
   `RawTextItem{text, bbox: quad.axis_rect(), source: Ocr, confidence: score}`；
   页 `coord_units = Some(Px{dpi})`；
   - engine 为 None 且存在 raster 页：mode=On/Auto → `Unsupported("第 N… 页
     需 OCR 引擎：开启 ocr-oar feature 或注入 OcrEngine")`（终止码）；
5. Off 的 raster 页：空 items + `meta["skipped_ocr"]=Bool(true)`（页级 meta
   无处放——放每块？页无 meta。落在 `RawDoc.meta["warnings"]`：
   `"扫描页未 OCR（mode=Off）：第 {pages} 页"`，引擎语义 warnings 已有通道）；
6. `RawDoc{pages, needs_ocr: !raster_pages.is_empty()}`。

`FormatPlugin`（id="pdf"）：default_parse_chain=["pdf-inspector"]，
assemble 沿用既有通用组装（items→Block 透传 source/confidence/coord_units）。

### 5.3 顶层便捷 API（src/lib.rs）

```rust
/// 便捷解析：默认 Registry（文本族内置插件 + pdf（若启用 feature 且提供引擎））。
/// 复杂装配（自定义引擎/选项）用 Registry + DocumentEngine。
pub fn parse(path: &str) -> Result<Document>;
```

默认装配规则：文本族内置；`pdf` feature 开启时注册 `PdfOptions::default()`
（engine：`ocr-oar` feature 开启时自动用 `OarOcrEngine::global()`，否则 None）。

## 6. 金测（src/backends/pdf.rs tests + 测试工具模块）

样例生成（`tests-support` 内部模块，参考 rumi sample 策略独立实现）：
- CJK 字体探测（msyh.ttc → Noto CJK ttc → WPS HYZYB5）+ ttc 抽面
  （'ttcf' 头解析 + 表目录 offset 重写，算法独立实现）+ 缺字形检查；
- `generate_text_pdf`：2 页 × 3 行中英混排；
- `generate_mixed_pdf`：页 1 文字层 + 页 2 白底黑条带 JPEG"扫描页"。

用例（测试名写死）：

| 测试 | 断言 |
| --- | --- |
| `text_layer_pdf_routes_all_pages_to_textlayer` | 2 页全 TextLayer；期望行相似度 ≥0.98（编辑距离归一化，独立实现）；bbox 自洽（x0≤x1, y0≤y1, 正坐标）；coord_units==Pt；confidence 全 None |
| `mixed_pdf_auto_without_engine_reports_unsupported` | 混合件 Auto 无引擎：Err 含"需 OCR 引擎"，code==Unsupported |
| `mixed_pdf_off_mode_skips_scan_pages` | Off：页 1 正常 TextLayer，页 2 无块；warnings 含"未 OCR"与页码 |
| `on_mode_requires_engine_for_all_pages` | 纯文字件 On 无引擎：Err"需 OCR 引擎" |
| `missing_file_error_is_unsupported` | 不存在路径：Err 含"文件不存在" |
| `pdftoppm_missing_has_friendly_error` | PATH 置空调 raster 路径：Err 含"找不到 pdftoppm" |
| `#[ignore] mixed_pdf_auto_ocr_end_to_end` | 混合件 Auto + OarOcrEngine（~/.oar 有缓存才不 ignore 跑）：页 2 产出 Ocr 块带 confidence>0、coord_units==Px{dpi:200}、条带区非空文本或至少块数>0 |

测试输出统一 `target/pdf-m1-{tag}-{pid}/`（rumi 纪律同款）。

## 7. 验收（门禁）

1. `cargo test`（default）全绿——**裸内核不含 pdf 代码**；
2. `cargo test --features pdf`：§6 表前 6 项绿（无引擎路径全覆盖）；
3. `cargo test --features ocr-oar -- --ignored`：OCR 端到端绿（需本机
   pdftoppm + ~/.oar 模型缓存；缺失则如实报告，不静默）；
4. `cargo tree --edges normal`（default）仍仅 serde；`--features pdf` 不含
   ort/pyo3/firecrawl-pdfium；
5. **与 rumi 对表**：同一 generate_text_pdf 样例，docforge 提取文本 vs
   rumi `route_pdf` 输出行集相似度（对表脚本一次性跑，结果记 docs/开发记录.md，
   差异列明）；
6. docs/开发记录.md 补 M1 段；THIRD_PARTY_NOTICES 无新增（pdf-inspector/
   oar-ocr 是依赖非吸收）。

## 8. 执行方式

单工人顺序实现：ocr/mod.rs → plugin.rs 扩展 → backends/pdf.rs → 顶层 API →
测试工具 → 用例 → 门禁 → 对表 → commit。
