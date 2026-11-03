# docforge M2 规格：文本族增强 + emit 三态 + cli（M2a）／DOCX·EPUB·HTML（M2b）

> 依据：docparse-plan §13 M2 行 + §2.4 吸收裁决 + §8.4。
> 用户加码要求（/loop）：API 方便扩展、方便 OCR 与翻译、方便直接解析与转换格式。
> M2a 为本轮；M2b（吸收 docparser-office/epub + B 五件套 + HTML 三修补）下一轮。

## M2a 范围（本轮）

1. **TXT 编码链**（feature `encoding`，dep:encoding_rs）：
   `decode()` 在 UTF-8 strict 失败后尝试 GB18030 → Big5（各以 had_errors==false
   才接受），未开 feature 保持 lossy 现状（docparser 注释同步更新）。
2. **TXT 中文章节切分**（新 `plugins/chapter.rs`，零依赖手写）：
   B 算法骨架（强弱类型 + 弱类型需相邻同类确认 + 确认数<2 且无强类型不分 +
   首边界 >60% 正文放弃）+ 中文词表（第[数]+[章回节卷幕篇部]、
   序章/序言/序幕/楔子/引子/番外/后记/尾声/终章/附录/前言、Chapter N、
   Roman `IV.`、Numbered `1.`）。标题行 ≤40 字符。
   TextParseCapability 集成：有章 → 标题产 `level=Some(1)` item；
   TextPlugin::assemble 把 `level` 映射为 Heading 块（原全 Paragraph）。
3. **emit 扩展**（`emit.rs`）：
   - `to_json`/`to_json_pretty`（feature `json`，dep:serde_json）；
   - `to_translation_units(&Document) -> Vec<TranslationUnit>`：翻译管线
     工作单元——`{id:"p{页}-b{块序}", page, kind, text, source, confidence,
     lang, chapter(最近上游 Heading 文本)}`；白名单 Heading/Paragraph/Quote/
     List；`min_chars` 过滤（默认 ≥2）。这是"方便翻译"的核心 API；
   - `to_markdown` 的 Image 块渲染为 `[image]`（+ ocr_text 有则附注）。
4. **lang-detect 处理能力**（feature `lang-detect`，dep:whatlang）：
   B 限流采样（前 1 万字符）→ `RawDoc.meta["language"]`；engine 通用搬运
   `RawDoc.meta["language"] → DocMeta.extra["language"]`（吸收文件 engine.rs
   的小改动，记入文件头）；TextPlugin.default_process_chain 按 feature 追加。
5. **cli**（`src/bin/docforge-cli.rs`，required-features=["json"]，手写参数
   解析不引 clap）：`docforge-cli <input> [--format md|json|text|units] 
   [--ocr auto|on|off] [--out <file>] [--pages a-b]`。

### M2a 用例（新增，名写死）

chapter.rs：`explicit_chinese_chapter_detected`（第一章/第二章 确认）、
`weak_numbered_needs_neighbor`（单个 "1. x" 不分章、两个相邻确认）、
`late_first_boundary_abandoned`（首边界 60% 后不分）、`special_headings`
（楔子/番外）、`no_chapters_single_flow`（普通段落不分）。
text.rs：`gb18030_decoded_when_feature`（GBK 字节 → 中文，#[cfg(feature=
"encoding)]）、`chapters_become_heading_blocks`（端到端 parse→Heading 块）。
emit.rs：`translation_units_carry_chapter_context`、`translation_units_skip_code`、
`image_block_renders_placeholder`、`to_json_roundtrip`。
lang_detect：`chinese_text_detected_zh`。
cli：冒烟（--format md 输出含 "# "）。

## M2b 范围（下一轮，另出增补节）

- DOCX：吸收 docparser-office/src/docx.rs（+styles.xml 样式名映射补丁）；
- EPUB：吸收 docparser-epub lib.rs + B 五件套融合（资源提取/img-src 重写/
  封面三级 fallback/linear=no/XML 编码嗅探）；
- HTML 扫描器三修补（属性引号 `>` 截断、`<img>`→Image、ol 有序）；
- 各格式金测（真实文件，含中文 DOCX/EPUB）。

## 验收（M2a 门禁）

`cargo test`（default）全绿；`--features encoding,json,lang-detect` 全绿；
default `cargo tree` 仍仅 serde；开发记录 M2a 段 + commit。
