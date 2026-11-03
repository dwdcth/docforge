# docforge

通用文档解析 crate：docparser 式统一块级 IR + 格式路由（magic 嗅探 + 后端回退链）
+ **PDF 页级混合路由**（逐页判定"文字层直提 vs 渲染+OCR"，设计参考 rumi-ocr 的
pdf_route）。OCR 置信度与块来源进 IR；输出 JSON / Markdown / 翻译友好分段，
服务解析转换、OCR、翻译三类消费者。独立项目，与 rumi-ocr 无依赖关系。

## 状态

| 期 | 内容 | 状态 |
| --- | --- | --- |
| M0 | 工程骨架 + docparser-core 吸收 + IR 定稿（confidence/source/coord_units） | ✅ |
| M1 | PDF 后端（页级路由 + oar-ocr OCR 接入） | — |
| M2 | 文本族增强（GBK/Big5 编码链、中文章节切分）+ DOCX + EPUB + emit + cli | — |
| M3 | XLSX/PPTX/ODF + RTF + 内嵌图片 OCR 链 | — |
| M4 | DjVu + 页内混合路由 + RAG 分块 emit | — |
| M5 | FB2/MOBI/CBZ 电子书扩展 | — |

规划：[docparse-plan.md](docparse-plan.md)；各期规格：`docforge-spec-*.md`。

## 许可

MIT OR Apache-2.0。吸收自 docparser 家族与 document-parser 的源码见
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)（源码以 crates.io 存档为准，
随库于 `vendor/*.crate`）。
