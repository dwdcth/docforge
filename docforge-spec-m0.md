# docforge M0 规格：工程骨架 + core 吸收 + IR 定稿

> 依据：docparse-plan.md §13（M0 行）+ §5（IR 四处修改）+ §2.4（吸收裁决）。
> 定位：独立项目（§1 定位修订），与 rumi-ocr 无依赖关系。
> 状态：待用户审 → 审过即开工。规格与计划冲突以本规格为准（计划 §0 约定）。

## 1. 范围

M0 交付四件事，其余不做：

1. 工程骨架：git 仓库 + crate 初始化（lib-only，无 bin）；
2. docparser-core 0.2.1 骨架吸收（model/plugin/registry/router/policy/engine/
   error/emit + plugins/*），crate 名与 IR 修改；
3. IR 定稿：§5 四处修改全部落地 + 单测锁定；
4. vendor 声明（THIRD_PARTY_NOTICES.md）+ Q3 核实（pdf-inspector 依赖面结论）。

**不做**（后续期次）：任何新格式后端、PDF/OCR 接入（M1）、emit 扩展与 cli（M2）、
office/epub 吸收（M2/M3）、翻译友好 emit。

## 2. 工程骨架（文件集与内容写死）

```
/mnt/s790/project/docforge/
├── .git/                    # git init（初始分支 main）
├── .gitignore               # /target、vendor/*/（解压目录；tarball 进 git）
├── Cargo.toml
├── Cargo.lock
├── THIRD_PARTY_NOTICES.md
├── README.md
├── docparse-plan.md         # 已存在，随库
├── docforge-spec-m0.md      # 本文件
├── docs/开发记录.md          # Q3 结论 + M0 验收记录
├── vendor/                  # 已存在：7 个 .crate tarball 进 git，解压目录 ignore
└── src/
    ├── lib.rs
    ├── model.rs
    ├── error.rs
    ├── plugin.rs
    ├── registry.rs
    ├── router.rs
    ├── policy.rs
    ├── engine.rs
    ├── emit.rs
    └── plugins/
        ├── mod.rs
        ├── text.rs
        ├── markdown.rs
        ├── csv.rs
        ├── html.rs
        ├── rtf.rs
        └── subtitle.rs
```

**Cargo.toml**（全文，逐字）：

```toml
[package]
name = "docforge"
version = "0.1.0"
edition = "2021"
description = "通用文档解析：统一块级 IR + 格式路由 + PDF 页级混合路由（文字层直提 / OCR），OCR 与翻译友好输出"
license = "MIT OR Apache-2.0"
keywords = ["document", "parser", "ocr", "ir", "pdf"]
categories = ["parser-implementations", "text-processing"]

[features]
default = ["md", "csv", "html", "rtf"]
md = []
csv = []
html = []
rtf = []
subtitle = []

[dependencies.serde]
version = "1"
features = ["derive"]

[dev-dependencies]
serde_json = "1"
```

依赖纪律（沿 docparser-core）：**运行时仅 serde**；error 手写（不引 thiserror）；
pdf-extract/encoding_rs 等一切后续依赖必须走 feature 门控（B 的 feature 组织先例，
计划 §2.4-8）。

**.gitignore**：

```
/target
/vendor/*/
```

（tarball `vendor/*.crate` 进 git——crates.io 存档是两家源码的唯一合法源，
随库保证可追溯与离线重建。）

**README.md**（骨架，一段话 + 状态表）：定位"通用文档解析 crate：docparser 式
块级 IR + 格式路由 + rumi 式 PDF 页级混合路由（文字层直提 vs 渲染 OCR），
OCR 置信度与来源进 IR，输出 JSON/Markdown/翻译友好分段"；状态 M0；指向
docparse-plan.md。

## 3. 吸收映射与改造点（逐文件）

来源：`vendor/docparser-core-0.2.1/src/`（MIT OR Apache-2.0）。除下表改动外
**逐字保留**（含原有测试与中文注释——它们是行为基线）：

| docforge 文件 | 来源 | 改动 |
| --- | --- | --- |
| lib.rs | lib.rs | crate 文档头改写（docforge 定位一句话 + 吸收声明）；`docparser_core` 引用全局替换为 `docforge`；plugins mod 声明按 feature 补 subtitle 门控 |
| model.rs | model.rs | **§4 四处修改**；模块头注释补 docforge 差异说明 |
| error.rs | error.rs | crate 名引用替换，其余不动 |
| plugin.rs | plugin.rs | 不动 |
| registry.rs | registry.rs | 不动 |
| router.rs | router.rs | 不动（pdf 嗅探位 M1 加） |
| policy.rs | policy.rs | 不动 |
| engine.rs | engine.rs | 不动（含 parse_txt_end_to_end 等全部测试） |
| emit.rs | emit.rs | 不动 |
| plugins/*.rs | 同名 | 文件头加吸收声明；subtitle.rs 若原无条件编译则包 `#[cfg(feature = "subtitle")]` |

每个吸收文件头加统一两行：

```rust
// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：<本表所列，无则写"仅 crate 名替换">。
```

## 4. IR 定稿（四处修改，代码写死）

### 4.1 `Block` 新增 `confidence`（OCR 行置信度）

```rust
    /// OCR 行置信度 0..1（`source = Ocr` 时有值；TextLayer/Native 为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
```

### 4.2 `Block` 新增 `source`（wire 兼容：缺省 = Native）

```rust
/// 块来源：消费者据此区分"零失真文本"（TextLayer/Native）与"识别文本"（Ocr）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockSource {
    /// PDF 文字层直提（PDF 点坐标、左下原点，契约见 M1 pdf 后端模块头）
    TextLayer,
    /// 渲染 + OCR 识别（置信度见 `Block.confidence`）
    Ocr,
    /// 原生结构（DOCX/EPUB 等格式自带，或 TXT 等按行重组）
    #[default]
    Native,
}
```

`Block` 内：

```rust
    /// 块来源。serde 缺省 Native：docparser 旧 JSON（无此字段）可无损读入。
    #[serde(default)]
    pub source: BlockSource,
```

wire 值：`"text_layer"` / `"ocr"` / `"native"`（snake_case；单测锁定）。

### 4.3 `Page` 新增 `coord_units`（坐标语义显式化）

```rust
/// 页面几何的坐标单位与语义（消费者换算依据；缺省 None = 无几何/未标注）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoordUnits {
    /// PDF 点（1/72 inch），左下原点、y 向上（PDF 文字层契约，随 pdf_route 头注释迁入）
    Pt,
    /// 渲染像素，左上原点；`dpi` 为渲染分辨率（oar-ocr 输入图）
    Px { dpi: f32 },
    /// 格式逻辑单位（docx twip/EMU 等）或流式无几何
    Logical,
}
```

`Page` 内：

```rust
    /// 本页 blocks/images 的 bbox 坐标单位。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coord_units: Option<CoordUnits>,
```

wire 形态：`{"type":"pt"}` / `{"type":"px","dpi":144.0}` / `{"type":"logical"}`。

### 4.4 `DocMeta` 不改结构

`extra` 沿用（BTreeMap<String,String>）；`file_type` 由 router 规范格式名回填
（engine.rs 既有逻辑，不动）。lang-detect 等 M2 落 `extra["language"]`，M0 无代码。

## 5. 单测清单（model.rs tests 新增 6 个，测试名与断言写死）

吸收的 6 个既有测试（finalize 计数 / blocks_to_text 表格展开 / meta 三形态
wire / 空 meta 省略 / BlockKind wire / roundtrip）必须原样绿。新增：

1. `block_source_wire_values`——TextLayer/Ocr/Native 序列化逐字等于
   `"text_layer"`/`"ocr"`/`"native"`，且反序列化往返。
2. `block_source_defaults_to_native`——`{"kind":"paragraph"}`（无 source 键）
   读入后 `source == BlockSource::Native`（docparser 旧 JSON 兼容）。
3. `block_confidence_serializes_only_when_present`——None 无 `confidence` 键；
   `Some(0.87)` 往返保 float。
4. `page_coord_units_wire`——三种变体的 wire 形态逐断言（含 Px 的
   `{"type":"px","dpi":144.0}`）。
5. `page_coord_units_omitted_when_none`——None 无 `coord_units` 键。
6. `roundtrip_json_with_ocr_fields`——含 confidence+source(Ocr)+coord_units(Px)
   的 Document serde_json 全量往返相等。

## 6. THIRD_PARTY_NOTICES.md（全文结构写死）

- 表格 7 行：docparser-{core,pdf,office,epub,odf,sdk} 0.2.1（M0 仅吸收 core，其余
  为 M2/M3 吸收源预先列示）、document-parser 0.1.0-alpha.18（M2/M4/M5 吸收源）。
  列：crate、版本、许可（均 MIT OR Apache-2.0）、存档 URL
  （`https://static.crates.io/crates/<name>/<name>-<ver>.crate`）、已吸收文件
  （M0：core 的 src/*.rs + src/plugins/*.rs）、备注（上游仓库 hi-agents/text-docparser
  已删 / LiiChar/document-parse 活跃）。
- 声明段：吸收文件头保留本 notices 指引；docparser 家族与 document-parser
  的版权与许可条款随 vendor tarball 内 LICENSE* 文件为准。

## 7. Q3 核实（M0 内完成，结论进 docs/开发记录.md）

`pdf-inspector 1.25`：`cargo add pdf-inspector --dry-run`（或临时壳 crate 解析
依赖树），回答三个问题：

1. firecrawl-pdfium / ort / pyo3 是否 default 依赖（有无 feature 可关）？
2. 纯文字层提取路径（`group_into_lines_preserving_all_text`）能否不拉 ort/pyo3？
3. 若不可关：M1 降级方案触发（pdfium-render 直连或 oxidize-pdf 提取，
   计划 §14 风险 2），把三方案依赖面对比表写进开发记录。

## 8. 验收（全部满足才过门禁）

1. `cargo check --all-features` 与 `cargo test`（default features）全绿；
2. §5 新增 6 测试存在且绿（CI 外本机跑）；
3. `git log` 初始 commit 包含：src/ + 全部规划文档 + vendor/*.crate（7 个）；
4. THIRD_PARTY_NOTICES.md 列全 7 来源；
5. docs/开发记录.md 有 Q3 结论与三问答案；
6. `cargo tree` 运行时依赖仅 serde（纪律不破）。

## 9. 执行方式

单工人即可（文件集无并行切分需求）；门禁照 §13 流程：cargo check/test +
审 diff（吸收文件 vs vendor 原文件的 diff 应只剩 §3 表所列改动）+ 修复轮。
