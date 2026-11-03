// docforge 新增（翻译闭环①写侧）：EPUB 双语回写。
//! EPUB 双语回写：ZIP 透传 + spine 各 XHTML 重块化对齐 + 定位改写。
//!
//! # 对齐口径（与 `backends/epub.rs` 提取侧逐条对应）
//!
//! * 提取侧管线：`EpubRawBackend`（spine 阅读序：linear="no" / .ncx / 缺件跳过）
//!   → `EpubPlugin::assemble`（每 spine 文档 `parse_blocks` 块化 → 按标题切节；
//!   单节文档坍缩成一段）→ `to_translation_units`。id 的页名来自该 Document
//!   （toc 标题或文档内标题），所以回写侧**重跑同一管线**拿权威 id/文本，
//!   再以同一套块化/切节口径把每个 Document 页映回「spine 文档 × raw 块序」：
//!   - 切节页（多节文档）：页内第 b 块 ↔ 该节的 raw 块下标（标题块被切节
//!     排除，与提取侧一致——这类标题本就不产翻译单元）；
//!   - 坍缩页（单节文档）：单元文本 = 全部非空块文本按 `\n\n` 连接；回写
//!     定位取**全部非空 raw 块**（BilingualInline 插在最后一块之后；
//!     TranslationOnly 首块首文本节点换译文、其余可翻译块的文本节点清空）。
//! * XHTML 定位用与 `plugins/html.rs::parse_blocks` 同口径的扫描（同款触发
//!   顺序 / 空白跳过 / 实体解码），带字节 span。简化取舍：BilingualInline
//!   一律插 `<p class="docforge-tr">`（heading 后也是 p；`</li>` 后插入落在
//!   列表容器内，阅读器容忍）；TranslationOnly 只动可翻译块的文本节点，
//!   Image/Code 结构块原样保留。
//! * 表格格级回写（wave2，与 `writeback::html` 同构，机制详见其模块头
//!   「表格格级回写」）：切节页的 Table 块在表格位逐格消费格单元——扫描的
//!   非空格序列与提取侧格单元同序同数（解析侧无 colspan 展开：td/th 文档序
//!   即列序），逐格配对 + 格文本守卫（不一致跳过该格不写，宁漏勿错）；
//!   TranslationOnly 格内首文本节点换译、其余清空，BilingualInline 格内
//!   `<br class="docforge-tr"/>` 分隔原/译，OriginalPlusAppendix 表格原样
//!   （格译文不进附录）。
//! * **单节坍缩边界**：`EpubPlugin::assemble`（Document 口径）把单节 spine
//!   文档坍缩成一段（`_xhtml_text` 回退同款，表格文本并入段落单元），该形态
//!   不产 Table 块 → 格单元不存在，格替换自动无操作（表格文本仍残留在坍缩
//!   段文本里）；多节（切节）文档表格保留结构块，格替换生效。
//! * 双保险：单元文本与重块化文本一致性比对，不一致跳过该单元（宁漏勿错）；
//!   非 UTF-8 的 spine 条目（罕见，如 GBK 声明件）不做改写（避免改写后
//!   字节与编码声明失配），其余条目照常透传。
//! * 未匹配 id / 空白译文一律忽略。

use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::backends::epub::{EpubPlugin, EpubRawBackend};
use crate::emit::BiStrategy;
use crate::error::{DocError, Result};
use crate::model::{Block, BlockKind};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc};

use super::html::{scan_html_blocks, HtmlSlot, TR_CLASS};
use super::{apply_edits, Edit, escape_xml_text, open_zip, rewrite_zip, translation_map};

/// epub 双语回写：见模块级文档。`translated` 的 id 与
/// [`crate::emit::to_translation_units`] 同口径（`p{页}-b{块序}`，页名取
/// Document 页名）。失败返回 `corrupt`。
pub fn writeback_epub(
    in_path: impl AsRef<Path>,
    translated: &[(String, String)],
    strategy: BiStrategy,
    out: impl AsRef<Path>,
) -> Result<()> {
    let in_path = in_path.as_ref();
    let out = out.as_ref();

    // A. 权威解析（提取侧同管线、同默认选项）拿 id / 文本
    let raw = EpubRawBackend::default().parse(in_path)?;
    let doc = EpubPlugin.assemble(&raw);
    // 单元流全量（含表格格单元 kind = Table）：块级候选按块序消费、表格格
    // 在表格位逐格消费（见主循环的 Table 分支），两段消费拼满同一单元流。
    let units = crate::emit::to_translation_units(&doc);
    let map = translation_map(translated);

    // B. spine 部件通道（与 EpubRawBackend::parse 的 spine 循环同款跳过规则）
    let mut parts = spine_parts(in_path)?;
    if parts.len() != raw.pages.len() {
        return Err(DocError::corrupt(
            "epub spine 对齐失败：部件通道与解析器页数不一致",
        ));
    }

    // C. 逐 spine 文档重块化（parse_blocks 同口径扫描，带 span）
    let mut slots: Vec<Vec<HtmlSlot>> = Vec::with_capacity(parts.len());
    let mut body_closes: Vec<Option<usize>> = Vec::with_capacity(parts.len());
    for part in &parts {
        let (s, bc) = scan_html_blocks(&part.xhtml);
        slots.push(s);
        body_closes.push(bc);
    }

    // D. Document 页 → raw 块 provenance（assemble 同口径）
    let prov = page_provenance(&raw, &slots)?;
    if prov.len() != doc.pages.len() {
        return Err(DocError::corrupt(
            "epub 对齐失败：provenance 页数与 Document 不一致",
        ));
    }

    // E. 单元 ↔（页, 页内块）与 units[] 严格同序（同一过滤口径），逐策略产编辑
    let mut edits: Vec<Vec<Edit>> = (0..parts.len()).map(|_| Vec::new()).collect();
    let mut cursor = 0usize;
    for (page_idx, page) in doc.pages.iter().enumerate() {
        let doc_i = prov[page_idx].doc;
        let raw_slots = &slots[doc_i];
        for (b_idx, b) in page.blocks.iter().enumerate() {
            if b.kind == BlockKind::Table {
                // 表格格级回写（wave2）：仅切节页有 Table 块（坍缩页的表格
                // 文本已并入段落单元，见模块头「单节坍缩边界」）。扫描格序列
                // ↔ 格单元顺序配对 + 文本守卫；OriginalPlusAppendix 表格原样
                // （格译文不进附录）。无 rows 结构的 Table 块防御性跳过。
                if b.table.is_none() {
                    continue;
                }
                let slot = &raw_slots[prov[page_idx].blocks[b_idx]];
                for cell in &slot.cells {
                    let Some(unit) = units.get(cursor) else { break };
                    cursor += 1;
                    // 双保险：格文本 ≠ 单元文本 → 该格不写（宁漏勿错；单元照常消费）
                    if unit.kind != BlockKind::Table || cell.text != unit.text {
                        continue;
                    }
                    let Some(tr) = map.get(unit.id.as_str()) else {
                        continue;
                    };
                    let tr = tr.trim();
                    if tr.is_empty() {
                        continue;
                    }
                    match strategy {
                        BiStrategy::BilingualInline => {
                            // 格内容终点（闭标签前）插 <br class="docforge-tr"/> + 译文
                            edits[doc_i].push(Edit::Insert(
                                cell.end,
                                format!("<br class=\"{TR_CLASS}\"/>{}", escape_xml_text(tr)),
                            ));
                        }
                        BiStrategy::TranslationOnly => {
                            // 格内首文本节点换译文、其余清空（与段落同款）
                            for (k, (s, e)) in cell.text_spans.iter().enumerate() {
                                if k == 0 {
                                    edits[doc_i]
                                        .push(Edit::Replace(*s, *e, escape_xml_text(tr)));
                                } else {
                                    edits[doc_i].push(Edit::Replace(*s, *e, String::new()));
                                }
                            }
                        }
                        BiStrategy::OriginalPlusAppendix => {}
                    }
                }
                continue;
            }
            let t = b.content.trim();
            if !crate::emit::is_translatable(b.kind) || t.is_empty() || t.chars().count() < 2 {
                continue;
            }
            let unit = &units[cursor];
            cursor += 1;
            let Some(tr) = map.get(unit.id.as_str()) else {
                continue;
            };
            let tr = tr.trim();
            if tr.is_empty() {
                continue;
            }
            // 单元 → 贡献 raw 块（切节页：单块；坍缩页：全部非空块）
            let (contrib, expect_text) = if prov[page_idx].collapsed {
                let idxs = prov[page_idx].blocks.clone();
                let joined = idxs
                    .iter()
                    .map(|&i| raw_slots[i].text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                (idxs, joined)
            } else {
                let i = prov[page_idx].blocks[b_idx];
                (vec![i], raw_slots[i].text.clone())
            };
            // 双保险：重块化文本必须与权威单元文本一致
            if expect_text != unit.text {
                continue;
            }
            match strategy {
                BiStrategy::BilingualInline => {
                    // 插在最后一块之后；无块回退的全文 span 钳进 </body> 前
                    let mut pos = raw_slots[contrib[contrib.len() - 1]].end;
                    if let Some(bc) = body_closes[doc_i] {
                        pos = pos.min(bc);
                    }
                    edits[doc_i].push(Edit::Insert(
                        pos,
                        format!("<p class=\"{TR_CLASS}\">{}</p>", escape_xml_text(tr)),
                    ));
                }
                BiStrategy::TranslationOnly => {
                    // 首个贡献块的首文本节点换译文；其余可翻译块的文本节点清空。
                    // Image/Code 结构块的文本不动；坍缩页贡献块中的 Table 槽位
                    // 同样不动（坍缩页无格单元，见模块头「单节坍缩边界」）。
                    let mut first = true;
                    for i in &contrib {
                        let slot = &raw_slots[*i];
                        if !matches!(
                            slot.kind,
                            BlockKind::Heading
                                | BlockKind::Paragraph
                                | BlockKind::Quote
                                | BlockKind::List
                        ) {
                            continue;
                        }
                        for (s, e) in &slot.text_spans {
                            if first {
                                edits[doc_i].push(Edit::Replace(*s, *e, escape_xml_text(tr)));
                                first = false;
                            } else {
                                edits[doc_i].push(Edit::Replace(*s, *e, String::new()));
                            }
                        }
                    }
                }
                BiStrategy::OriginalPlusAppendix => {
                    // 末尾统一追加（见下）
                }
            }
        }
    }

    // Appendix：正文不动，最后 spine 文档 `</body>` 前追加「译文」小节
    if strategy == BiStrategy::OriginalPlusAppendix && !units.is_empty() && !parts.is_empty() {
        let matched: Vec<&str> = units
            .iter()
            // 表格格译文不进附录（附录是段落流；表格正文原样）
            .filter(|u| u.kind != BlockKind::Table)
            .filter_map(|u| map.get(u.id.as_str()))
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect();
        if !matched.is_empty() {
            let doc_i = parts.len() - 1;
            let mut appendix = String::from("<h2>译文</h2>");
            for tr in matched {
                appendix.push_str(&format!("<p>{}</p>", escape_xml_text(tr)));
            }
            let pos = body_closes[doc_i].unwrap_or(parts[doc_i].xhtml.len());
            edits[doc_i].push(Edit::Insert(pos, appendix));
        }
    }

    // F. 改写并重打包（非 UTF-8 条目不改写，防编码声明失配）
    let mut replaced: Vec<(String, Vec<u8>)> = Vec::new();
    for (i, part) in parts.iter_mut().enumerate() {
        if edits[i].is_empty() {
            continue;
        }
        if std::str::from_utf8(&part.bytes).is_err() {
            continue;
        }
        part.xhtml = apply_edits(&part.xhtml, std::mem::take(&mut edits[i]));
        replaced.push((part.name.clone(), part.xhtml.clone().into_bytes()));
    }
    rewrite_zip(in_path, out, &replaced)
}

// ---------------------------------------------------------------------------
// Document 页 → raw 块 provenance（assemble_impl 同口径的复刻）
// ---------------------------------------------------------------------------

/// 一个 Document 页的归属：来自哪个 spine 文档、覆盖哪些 raw 块。
struct PageProv {
    /// 所属 spine 文档下标（= raw.pages / parts 下标）。
    doc: usize,
    /// 是否单节坍缩页（Document 页只有一段合并文本）。
    collapsed: bool,
    /// 页对应的 raw 块下标（坍缩页 = 全部非空块；切节页 = 该节的块下标序列）。
    blocks: Vec<usize>,
}

/// 复刻 `EpubPlugin::assemble_impl` 的分页判定：纯图/空白页不成章；
/// `sections`（层级自适应切节）多节 → 每节一页；单节 → 坍缩页。
fn page_provenance(raw: &RawDoc, slots: &[Vec<HtmlSlot>]) -> Result<Vec<PageProv>> {
    let mut out = Vec::new();
    for (doc_i, rp) in raw.pages.iter().enumerate() {
        let Some(_xhtml) = rp.items.first().map(|i| i.text.as_str()) else {
            continue;
        };
        let raw_slots = &slots[doc_i];
        if raw_slots.iter().all(|s| s.text.is_empty()) {
            continue; // 纯图/空白页不成章（与 assemble 同口径）
        }
        // 复用提取侧的切节函数（层级自适应 h2→h3→h4 必须同源）
        let blocks_for_cut: Vec<Block> = raw_slots
            .iter()
            .map(|s| Block {
                kind: s.kind,
                content: s.text.clone(),
                level: s.level,
                ..Default::default()
            })
            .collect();
        let secs = crate::backends::epub::sections(&blocks_for_cut);
        if secs.len() > 1 {
            for (_title, idxs) in secs {
                out.push(PageProv {
                    doc: doc_i,
                    collapsed: false,
                    blocks: idxs,
                });
            }
            continue;
        }
        out.push(PageProv {
            doc: doc_i,
            collapsed: true,
            blocks: (0..raw_slots.len())
                .filter(|&i| !raw_slots[i].text.is_empty())
                .collect(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// spine 部件通道（镜像 backends/epub.rs 的容器→OPF→spine 循环）
// ---------------------------------------------------------------------------

struct SpinePart {
    /// 包内路径（ZIP 条目名）。
    name: String,
    /// 原始字节（UTF-8 判定用）。
    bytes: Vec<u8>,
    /// 与提取侧 `decode_entry` 同口径解码出的 XHTML。
    xhtml: String,
}

/// 条目解码（镜像 `backends/epub.rs::decode_entry`）：
/// UTF-8 直取 → XML 声明嗅探（`encoding` feature 下 encoding_rs）→ 统一解码链兜底。
fn decode_entry(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let head = &bytes[..bytes.len().min(1024)];
    let head_lossy = String::from_utf8_lossy(head);
    if let Some(enc) = head_lossy
        .split("encoding")
        .nth(1)
        .and_then(|rest| {
            rest.trim_start()
                .strip_prefix('=')
                .map(|r| r.trim_start().trim_start_matches('"').trim_start_matches('\''))
        })
        .and_then(|r| r.split(['"', '\'', ' ', '?']).next())
        .map(str::to_ascii_lowercase)
    {
        #[cfg(feature = "encoding")]
        {
            for label in [&enc, &enc.replace('_', "-")] {
                if let Some(cs) = encoding_rs::Encoding::for_label(label.as_bytes()) {
                    if cs == encoding_rs::UTF_8 {
                        break;
                    }
                    let (s, _, had_err) = cs.decode(bytes);
                    if !had_err {
                        return s.into_owned();
                    }
                }
            }
        }
        let _ = enc;
    }
    crate::encoding::decode(bytes.to_vec())
}

fn read_entry_bytes(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    match zip.by_name(name) {
        Ok(mut f) => {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut buf)
                .map_err(|e| DocError::corrupt(format!("读取 {name} 失败: {e}")))?;
            Ok(Some(buf))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(DocError::corrupt(format!("ZIP 读取 {name} 失败: {e}"))),
    }
}

/// spine 阅读序的部件列表：linear="no" / manifest 缺项 / .ncx / 缺条目一律跳过
/// （与 `EpubRawBackend::parse` 同款规则，保证与 raw.pages 一一同位）。
fn spine_parts(path: &Path) -> Result<Vec<SpinePart>> {
    let mut zip = open_zip(path)?;
    let container = read_entry_bytes(&mut zip, "META-INF/container.xml")?
        .map(|b| decode_entry(&b))
        .ok_or_else(|| DocError::corrupt("EPUB 缺 META-INF/container.xml"))?;
    let opf_path = crate::backends::epub::find_rootfile(&container)?
        .ok_or_else(|| DocError::corrupt("container.xml 缺 rootfile"))?;
    let opf_xml = read_entry_bytes(&mut zip, &opf_path)?
        .map(|b| decode_entry(&b))
        .ok_or_else(|| DocError::corrupt(format!("EPUB 缺 OPF: {opf_path}")))?;
    let (items, spine) = parse_opf_spine(&opf_xml)?;
    let opf_dir = opf_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

    let mut parts = Vec::new();
    for (idref, linear) in spine {
        if !linear {
            continue;
        }
        let Some(href) = items.iter().find(|(id, _)| *id == idref).map(|(_, h)| h) else {
            continue;
        };
        let full = crate::backends::epub::resolve(opf_dir, href);
        if full.ends_with(".ncx") {
            continue;
        }
        let Some(bytes) = read_entry_bytes(&mut zip, &full)? else {
            continue;
        };
        parts.push(SpinePart {
            name: full,
            bytes: bytes.clone(),
            xhtml: decode_entry(&bytes),
        });
    }
    Ok(parts)
}

/// OPF 只取 manifest `(id, href)` 与 spine `(idref, linear)`（导航/封面与回写无关）。
#[allow(clippy::type_complexity)]
fn parse_opf_spine(xml: &str) -> Result<(Vec<(String, String)>, Vec<(String, bool)>)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut items = Vec::new();
    let mut spine = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name = e.local_name().as_ref().to_string();
                let attr = |k: &str| {
                    e.attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == k)
                        .map(|a| a.value.to_string())
                };
                match name.as_str() {
                    "item" => {
                        if let (Some(id), Some(href)) = (attr("id"), attr("href")) {
                            items.push((id, href));
                        }
                    }
                    "itemref" => {
                        if let Some(idref) = attr("idref") {
                            let linear = attr("linear").map(|v| v != "no").unwrap_or(true);
                            spine.push((idref, linear));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("OPF 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok((items, spine))
}

// ---------------------------------------------------------------------------
// XHTML 候选位扫描：已抽到 writeback::html 共用（本文件逐 spine 文档调用，
// html 单文件回写对整篇调用一次）——见 super::html::scan_html_blocks / HtmlSlot。
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::path::PathBuf;

    use crate::backends::epub::read_epub;
    use crate::emit::to_translation_units;
    use crate::model::BlockKind::{Heading, List, Paragraph, Table};

    const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

    const NAV: &str = r#"<?xml version="1.0"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>
<nav epub:type="toc" xmlns:epub="http://www.idpf.org/2007/ops">
  <ol>
    <li><a href="text/ch1.xhtml">第一章</a></li>
    <li><a href="text/ch2.xhtml">第二章</a></li>
  </ol>
</nav></body></html>"#;

    /// ch1：单节文档（坍缩页）；含表格——坍缩形态下表格文本并入段落单元、
    /// 格单元不存在（见模块头「单节坍缩边界」）。
    const CH1: &str = "<html><body><h1>第一章</h1><p>正文一。</p><p>正文二。</p>\
<table><tr><td>格甲</td><td>格乙</td></tr></table></body></html>";
    /// ch2：两个 h2 → 切节页。
    const CH2: &str = "<html><body><h2>甲节</h2><p>甲正文，段落测试。</p>\
<h2>乙节</h2><ul><li>要点一。</li><li>要点二。</li></ul></body></html>";
    /// ch3：两个 h2 → 切节页；首节含表格（格级回写目标，colspan 行无展开）。
    const CH3: &str = "<html><body><h2>参数节</h2><p>如下表所示。</p>\
<table><tr><th>名称</th><th>单位</th></tr>\
<tr><td>电压</td><td>伏</td></tr>\
<tr><td colspan=\"2\">跨列说明</td></tr></table>\
<h2>结论节</h2><p>综上各条。</p></body></html>";

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_epub_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// N 章 epub：manifest/spine 按 chs 动态生成（ch{i} → text/ch{i}.xhtml；
    /// 章名 nav 只给前两章，后续章页名走文档内标题口径）。
    fn build_epub_parts(dir: &Path, chs: &[&str]) -> PathBuf {
        let path = dir.join("book.epub");
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        let mut put = |name: &str, body: &str| {
            zw.start_file(name, opts).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        };
        let items = (1..=chs.len())
            .map(|i| {
                format!(
                    r#"<item id="ch{i}" href="text/ch{i}.xhtml" media-type="application/xhtml+xml"/>"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n    ");
        let spine = (1..=chs.len())
            .map(|i| format!(r#"<itemref idref="ch{i}"/>"#))
            .collect::<Vec<_>>()
            .join("\n    ");
        let opf = format!(
            r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>测试书</dc:title>
    <dc:language>zh</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    {items}
  </manifest>
  <spine>
    {spine}
  </spine>
</package>"#
        );
        put("mimetype", "application/epub+zip");
        put("META-INF/container.xml", CONTAINER);
        put("OEBPS/content.opf", &opf);
        put("OEBPS/nav.xhtml", NAV);
        for (i, ch) in chs.iter().enumerate() {
            put(&format!("OEBPS/text/ch{}.xhtml", i + 1), ch);
        }
        zw.finish().unwrap();
        path
    }

    fn build_epub(dir: &Path, ch1: &str, ch2: &str) -> PathBuf {
        build_epub_parts(dir, &[ch1, ch2])
    }

    /// 提取侧同管线（EpubRawBackend + EpubPlugin::assemble）。
    fn parse_doc(path: &Path) -> crate::model::Document {
        let raw = EpubRawBackend::default()
            .parse(&path)
            .unwrap();
        EpubPlugin.assemble(&raw)
    }

    fn unit_ids(path: &Path) -> Vec<String> {
        to_translation_units(&parse_doc(path))
            .into_iter()
            .map(|u| u.id)
            .collect()
    }

    fn write(in_path: &Path, out_path: &Path, tr: &[(&str, &str)], strategy: BiStrategy) {
        let translated: Vec<(String, String)> =
            tr.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        writeback_epub(in_path, &translated, strategy, out_path).unwrap();
    }

    /// 金测（核心验收）：BilingualInline 往返 —— 坍缩页插在最后非空块后、
    /// 切节页逐块原/译交错、表格不受扰、未匹配 id 忽略、mimetype 仍居首。
    #[test]
    fn roundtrip_bilingual_inline() {
        let dir = case_dir("inline");
        let in_path = build_epub(&dir, CH1, CH2);
        let out_path = dir.join("out.epub");

        // 提取侧 id 口径（页名 = 章名；乙节两个 li 各一单元）
        assert_eq!(
            unit_ids(&in_path),
            ["p第一章-b0", "p甲节-b0", "p乙节-b0", "p乙节-b1"]
        );

        write(
            &in_path,
            &out_path,
            &[
                ("p第一章-b0", "Chapter 1"),
                ("p甲节-b0", "Section A body."),
                ("p乙节-b0", "Point one."),
                ("p乙节-b1", "Point two."),
                ("p不存在-b0", "bogus"), // 未匹配 id：忽略
            ],
            BiStrategy::BilingualInline,
        );

        // 书 API（保留结构化块）：原/译交错
        let book = read_epub(&out_path).unwrap();
        let titles: Vec<&str> = book.chapters.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["第一章", "甲节", "乙节"]);

        let ch1 = &book.chapters[0].blocks;
        let kinds: Vec<BlockKind> = ch1.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, [Heading, Paragraph, Paragraph, Table, Paragraph]);
        assert_eq!(ch1[3].content, "格甲 | 格乙", "表格不受扰");
        assert_eq!(
            ch1[3].table.as_ref().unwrap().rows,
            vec![vec!["格甲".to_string(), "格乙".to_string()]]
        );
        assert_eq!(ch1[4].content, "Chapter 1", "坍缩页译文插在最后非空块（表格）后");

        let jia = &book.chapters[1].blocks;
        assert_eq!(
            jia.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            ["甲正文，段落测试。", "Section A body."]
        );
        let yi = &book.chapters[2].blocks;
        assert_eq!(
            yi.iter().map(|b| (b.kind, b.content.as_str())).collect::<Vec<_>>(),
            [
                (List, "要点一。"),
                (Paragraph, "Point one."),
                (List, "要点二。"),
                (Paragraph, "Point two."),
            ]
        );

        // mimetype 仍为首条目且 Stored；条目数不变
        let mut zin = open_zip(&in_path).unwrap();
        let mut zout = open_zip(&out_path).unwrap();
        assert_eq!(zout.len(), zin.len());
        assert_eq!(zout.by_index(0).unwrap().name().unwrap(), "mimetype");
        assert_eq!(
            zout.by_index(0).unwrap().compression(),
            zip::CompressionMethod::Stored
        );
        // 未改写的条目字节透传（nav.xhtml）
        let mut a = String::new();
        let mut b = String::new();
        zin.by_name("OEBPS/nav.xhtml")
            .unwrap()
            .read_to_string(&mut a)
            .unwrap();
        zout.by_name("OEBPS/nav.xhtml")
            .unwrap()
            .read_to_string(&mut b)
            .unwrap();
        assert_eq!(a, b);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TranslationOnly：坍缩页首块换译文、其余可翻译块清空（Document 口径：
    /// 页文本 = 译文）；切节页逐块替换；未翻译单元原样。
    #[test]
    fn translation_only_replaces_text() {
        let dir = case_dir("tonly");
        // ch1 不放表格：坍缩单元的全部贡献块都是可翻译块（简化口径的干净路径）
        let ch1 = "<html><body><h1>第一章</h1><p>正文一。</p><p>正文二。</p></body></html>";
        let in_path = build_epub(&dir, ch1, CH2);
        let out_path = dir.join("out.epub");
        write(
            &in_path,
            &out_path,
            &[
                ("p第一章-b0", "Chapter 1"),
                ("p甲节-b0", "Section A body."),
                ("p乙节-b0", "Point one."),
            ],
            BiStrategy::TranslationOnly,
        );

        let doc = parse_doc(&out_path);
        assert_eq!(doc.pages.len(), 3);
        // 坍缩页：h1 换译文、两个 p 清空 → 页块 = 单段译文
        assert_eq!(doc.pages[0].name.as_deref(), Some("第一章"));
        assert_eq!(doc.pages[0].blocks.len(), 1);
        assert_eq!(doc.pages[0].blocks[0].content, "Chapter 1");
        // 切节页：逐块替换
        assert_eq!(doc.pages[1].blocks[0].content, "Section A body.");
        assert_eq!(doc.pages[2].blocks[0].content, "Point one.");
        assert_eq!(doc.pages[2].blocks[1].content, "要点二。", "未翻译单元原样");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全部 id 未匹配：输出与输入的 Document 解析逐块一致（透传）。
    #[test]
    fn unmatched_ids_leave_book_untouched() {
        let dir = case_dir("unmatched");
        let in_path = build_epub(&dir, CH1, CH2);
        let out_path = dir.join("out.epub");
        write(
            &in_path,
            &out_path,
            &[("p不存在-b9", "nope")],
            BiStrategy::BilingualInline,
        );
        assert_eq!(parse_doc(&out_path), parse_doc(&in_path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：正文不动，最后 spine 文档 </body> 前追加
    /// 「译文」小节（h2 标题 + 全部译文段，单元序）。
    #[test]
    fn appendix_appends_to_last_spine_doc() {
        let dir = case_dir("appendix");
        let in_path = build_epub(&dir, CH1, CH2);
        let out_path = dir.join("out.epub");
        write(
            &in_path,
            &out_path,
            &[
                ("p第一章-b0", "Chapter 1"),
                ("p甲节-b0", "Section A body."),
                ("p乙节-b0", "Point one."),
                ("p乙节-b1", "Point two."),
            ],
            BiStrategy::OriginalPlusAppendix,
        );

        // 前两个 spine 文档不动
        let raw_in = EpubRawBackend::default()
            .parse(&in_path)
            .unwrap();
        let raw_out = EpubRawBackend::default()
            .parse(&out_path)
            .unwrap();
        assert_eq!(raw_in.pages[0].items[0].text, raw_out.pages[0].items[0].text);

        // ch2 末尾追加 h2「译文」+ 4 段
        let ch2_out = &raw_out.pages[1].items[0].text;
        assert!(ch2_out.contains("<h2>译文</h2>"));
        for tr in ["Chapter 1", "Section A body.", "Point one.", "Point two."] {
            assert!(ch2_out.contains(&format!("<p>{tr}</p>")), "{tr} in appendix");
        }
        assert!(ch2_out.contains("要点一。"), "正文不动");
        let body_end = ch2_out.rfind("</body>").unwrap();
        let h2 = ch2_out.find("<h2>译文</h2>").unwrap();
        assert!(h2 < body_end, "附录在 </body> 之前");
        // 重解析：原三页内容不变；追加的 h2「译文」自开新节 → 第 4 页
        let doc = parse_doc(&out_path);
        assert_eq!(doc.pages.len(), 4);
        assert_eq!(doc.pages[0].blocks[0].content, "第一章\n\n正文一。\n\n正文二。\n\n格甲 | 格乙");
        assert_eq!(doc.pages[1].blocks[0].content, "甲正文，段落测试。");
        assert_eq!(doc.pages[2].blocks[0].content, "要点一。");
        assert_eq!(doc.pages[2].blocks[1].content, "要点二。");
        assert_eq!(doc.pages[3].name.as_deref(), Some("译文"));
        assert_eq!(
            doc.pages[3]
                .blocks
                .iter()
                .map(|b| b.content.as_str())
                .collect::<Vec<_>>(),
            ["Chapter 1", "Section A body.", "Point one.", "Point two."]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 表格格级回写（wave2 金测，切节页）：TranslationOnly 全格替换——格单元
    /// 全译后重解析，格文本 == 译文；colspan 行无展开（跨列文本落 (r,0)、
    /// 无续位）；单节坍缩页（ch1）不产格单元，表格文本随坍缩段原样。
    #[test]
    fn table_cells_translation_only_sectioned() {
        let dir = case_dir("cell-tonly");
        let in_path = build_epub_parts(&dir, &[CH1, CH2, CH3]);
        let out_path = dir.join("out.epub");

        // ch1 坍缩：无格单元（坍缩边界）；ch3 切节：表格按格展开，
        // 空格不发单元、单字符格照收、colspan 行只占 (r,0)
        assert_eq!(
            unit_ids(&in_path),
            [
                "p第一章-b0",
                "p甲节-b0",
                "p乙节-b0",
                "p乙节-b1",
                "p参数节-b0",
                "p参数节-b1-r0c0",
                "p参数节-b1-r0c1",
                "p参数节-b1-r1c0",
                "p参数节-b1-r1c1",
                "p参数节-b1-r2c0",
                "p结论节-b0",
            ]
        );

        write(
            &in_path,
            &out_path,
            &[
                ("p参数节-b0", "See the table."),
                ("p参数节-b1-r0c0", "Name"),
                ("p参数节-b1-r0c1", "Unit"),
                ("p参数节-b1-r1c0", "Voltage"),
                ("p参数节-b1-r1c1", "V"),
                ("p参数节-b1-r2c0", "Spanning note"),
                ("p结论节-b0", "As shown above."),
            ],
            BiStrategy::TranslationOnly,
        );

        let doc = parse_doc(&out_path);
        assert_eq!(doc.pages.len(), 5);
        // ch1 坍缩页：未喂译文，表格文本随坍缩段原样
        assert_eq!(
            doc.pages[0].blocks[0].content,
            "第一章\n\n正文一。\n\n正文二。\n\n格甲 | 格乙"
        );
        // 参数节（切节页）：段落替换 + 表格格全替换（colspan 行落 (r,0) 单格）
        assert_eq!(doc.pages[3].blocks[0].content, "See the table.");
        let rows = &doc.pages[3].blocks[1].table.as_ref().unwrap().rows;
        assert_eq!(
            rows,
            &vec![
                vec!["Name".to_string(), "Unit".to_string()],
                vec!["Voltage".to_string(), "V".to_string()],
                vec!["Spanning note".to_string()],
            ]
        );
        assert_eq!(doc.pages[4].blocks[0].content, "As shown above.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BilingualInline 格内对照 + OriginalPlusAppendix 附录排除（切节页）：
    /// 格内 `<br class="docforge-tr"/>` 分隔原/译（重解析为原/译直连拼接）、
    /// 未命中格原样；格译文不进附录（附录只收段落类）、表格原样。
    #[test]
    fn table_cells_inline_and_appendix_sectioned() {
        let dir = case_dir("cell-inline");
        let in_path = build_epub_parts(&dir, &[CH1, CH2, CH3]);

        // BilingualInline：只给电压格译文 → 该格 br 对照，其余格原样
        let out_path = dir.join("inline.epub");
        write(
            &in_path,
            &out_path,
            &[("p参数节-b1-r1c0", "Voltage")],
            BiStrategy::BilingualInline,
        );
        let raw_out = EpubRawBackend::default()
            .parse(&out_path)
            .unwrap();
        let ch3 = &raw_out.pages[2].items[0].text;
        assert!(
            ch3.contains(r#"<td>电压<br class="docforge-tr"/>Voltage</td>"#),
            "格内 br 对照插在格内容终点：{ch3}"
        );
        assert!(ch3.contains("<td>伏</td>"), "未命中格原样：{ch3}");
        let book = read_epub(&out_path).unwrap();
        let tbl = book.chapters[3].blocks[1].table.as_ref().unwrap();
        assert_eq!(
            tbl.rows[1],
            vec!["电压Voltage".to_string(), "伏".to_string()],
            "重解析：格文本原/译直连（br 不拆格）"
        );
        assert_eq!(tbl.rows[0], vec!["名称".to_string(), "单位".to_string()]);

        // OriginalPlusAppendix：格译文不进附录（附录只收段落类），表格原样
        let out2 = dir.join("appendix.epub");
        write(
            &in_path,
            &out2,
            &[
                ("p参数节-b0", "See the table."),
                ("p参数节-b1-r1c0", "Voltage"),
            ],
            BiStrategy::OriginalPlusAppendix,
        );
        let raw_out2 = EpubRawBackend::default()
            .parse(&out2)
            .unwrap();
        // 附录落在最后 spine 文档（ch3，表格所在部件）；表格原样、格译文
        // 不入正文也不入附录（附录只收段落类）
        let raw_in = EpubRawBackend::default()
            .parse(&in_path)
            .unwrap();
        let ch3_out = &raw_out2.pages[2].items[0].text;
        assert!(ch3_out.contains("<p>See the table.</p>"), "段落译文进附录");
        assert!(!ch3_out.contains("Voltage"), "格译文不进附录：{ch3_out}");
        assert!(ch3_out.contains("<td>电压</td>"), "表格原样");
        let body_end = ch3_out.rfind("</body>").unwrap();
        let h2 = ch3_out.find("<h2>译文</h2>").unwrap();
        assert!(h2 < body_end, "附录在 </body> 之前");
        // 其余 spine 文档不动
        assert_eq!(raw_in.pages[0].items[0].text, raw_out2.pages[0].items[0].text);
        assert_eq!(raw_in.pages[1].items[0].text, raw_out2.pages[1].items[0].text);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
