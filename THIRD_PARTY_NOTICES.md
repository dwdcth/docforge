# 第三方来源声明

docforge 吸收了下表 crate 的源码（吸收文件头部均有标注）。源码以 crates.io
存档 tarball 为准（随库于 `vendor/*.crate`，解压命令
`tar xzf <archive> -C vendor/<name> --strip-components=1`）；版权与许可条款以
各存档内的 LICENSE / LICENSE-APACHE / LICENSE-MIT 文件为准。

| crate | 版本 | 许可 | 存档 URL | 已吸收（期次） | 备注 |
| --- | --- | --- | --- | --- | --- |
| docparser-core | 0.2.1 | MIT OR Apache-2.0 | https://static.crates.io/crates/docparser-core/docparser-core-0.2.1.crate | src/{model,error,plugin,registry,router,policy,engine,emit}.rs、src/plugins/*（M0） | 上游 hi-agents/text-docparser 仓库已删（404），存档为唯一合法源 |
| docparser-pdf | 0.2.1 | MIT OR Apache-2.0 | https://static.crates.io/crates/docparser-pdf/docparser-pdf-0.2.1.crate | （未吸收；M1 参考） | 同上 |
| docparser-office | 0.2.1 | MIT OR Apache-2.0 | https://static.crates.io/crates/docparser-office/docparser-office-0.2.1.crate | （未吸收；M2 docx / M3 xlsx·pptx 吸收） | 同上 |
| docparser-epub | 0.2.1 | MIT OR Apache-2.0 | https://static.crates.io/crates/docparser-epub/docparser-epub-0.2.1.crate | （未吸收；M2 吸收） | 同上 |
| docparser-odf | 0.2.1 | MIT OR Apache-2.0 | https://static.crates.io/crates/docparser-odf/docparser-odf-0.2.1.crate | （未吸收；M3 吸收） | 同上 |
| docparser-sdk | 0.2.1 | MIT OR Apache-2.0 | https://static.crates.io/crates/docparser-sdk/docparser-sdk-0.2.1.crate | （facade，仅架构参考） | 同上 |
| document-parser | 0.1.0-alpha.18 | MIT OR Apache-2.0 | https://static.crates.io/crates/document-parser/document-parser-0.1.0-alpha.18.crate | （未吸收；M2 TXT 分章骨架/EPUB 五件套、M4 DjVu 模式、M5 FB2 吸收） | 上游 LiiChar/document-parse 活跃；其 DOCX 因中文内容丢失级缺陷不采（docparse-plan §2.4） |

其余依赖（serde 及后续 pdf-inspector / oar-ocr / encoding_rs 等）按各自许可
正常依赖，随 cargo metadata 自动携带。

rumi-ocr（MulanPSL-2.0）的 pdf_route / dual_pdf 仅作设计参考，docforge 内
重新实现，不复制其源码（docparse-plan §12）。
